use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::drop::drop_agent;
use crate::agent::spawn::{spawn_agent, spawn_manual_child, SpawnRequest};
use crate::agent::{AgentId, AgentRegistry, AgentRole};
use crate::config::Config;
use crate::git::{dir_basename, GitClient};
use crate::ipc::protocol::{OkBody, Request, Response, MAX_SPAWN_TASK_BYTES};
use crate::session::SessionManager;

/// Shared context injected into every IPC handler.
#[derive(Clone)]
pub struct AppCtx {
    pub registry: Arc<AgentRegistry>,
    pub sessions: Arc<SessionManager>,
    pub socket: PathBuf,
    pub git: Arc<GitClient>,
    pub config: Arc<Config>,
    /// When true, the server spawns a background task that drains agent PTY
    /// exits and marks them Error. Set false in mock/test mode.
    pub watch_sessions: bool,
    /// Notified once by `Request::Shutdown`'s handler, after its response has
    /// already been written back to the caller. `ipc::server::run`'s accept
    /// loop selects on this to stop accepting connections and return, letting
    /// the daemon process exit by simply reaching the end of `main` — no
    /// `std::process::exit` needed, so a still-in-flight response (this
    /// request's own) is never raced against the runtime tearing down.
    pub shutdown_notify: Arc<tokio::sync::Notify>,
}

/// Dispatches a parsed request to the registry and returns a wire response.
/// No socket I/O occurs here — this is the unit-testable seam.
/// Blocking calls (git, session launch) are expected to run inside `spawn_blocking` at the call site.
pub fn dispatch(ctx: &AppCtx, req: Request) -> Response {
    match req {
        Request::Status { agent_id, status, message, context_pct, adapter, pushed_at } => {
            match ctx.registry.set_status(&agent_id, status, message, context_pct, adapter, pushed_at) {
                Ok(()) => Response::ok(None),
                Err(e) => Response::err(e.to_string()),
            }
        }

        Request::List => Response::ok(Some(OkBody::Agents { agents: ctx.registry.snapshot() })),

        Request::Agent { agent_id } => match ctx.registry.get(&agent_id) {
            Some(agent) => Response::ok(Some(OkBody::Agent { agent })),
            None => Response::err(format!("unknown agent: {}", agent_id.short())),
        },

        Request::Start { cwd, adapter } => {
            let cwd = cwd.unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
            });

            // A typo'd or missing path must still hard-fail -- unlike a git
            // failure below, there's no honest name to fall back to here.
            if !cwd.is_dir() {
                return Response::err(format!("not a directory: {}", cwd.display()));
            }

            // Not every root lives in a git repo. Fall back to the directory's
            // own basename and an explicit empty branch rather than silently
            // registering a phantom root under faked values (e.g. repo="unknown",
            // branch="main") -- see 8e10c71, which this relaxes.
            let (repo, branch) = match (ctx.git.repo_name(&cwd), ctx.git.current_branch(&cwd)) {
                (Ok(repo), Ok(branch)) => (repo, branch),
                _ => (dir_basename(&cwd), String::new()),
            };

            let req = SpawnRequest {
                role: AgentRole::Root,
                parent_id: None,
                task: String::new(),         // ignored for Root — a chosen adapter still gets no task
                name: None,                  // ignored for Root — always named after the repo
                adapter_name: String::new(), // ignored for Root — see root_adapter instead
                cwd,
                repo,
                branch: Some(branch),
                root_adapter: adapter,
            };

            match spawn_agent(&ctx.registry, &ctx.sessions, &ctx.socket, &ctx.config, req) {
                Ok(result) => Response::ok(Some(OkBody::Registered {
                    agent_id: result.id,
                    branch: result.branch,
                })),
                Err(e) => Response::err(e.to_string()),
            }
        }

        Request::Spawn { parent_id, task, name, adapter, cwd } => {
            if task.len() > MAX_SPAWN_TASK_BYTES {
                return Response::err(format!(
                    "task exceeds max size of {MAX_SPAWN_TASK_BYTES} bytes"
                ));
            }
            let parent = match spawn_parent(ctx, &parent_id) {
                Ok(parent) => parent,
                Err(response) => return response,
            };
            // Default to the spawning agent's *own* adapter, not a fixed
            // global default (AGENTS.md: cross-harness spawning is opt-in
            // via an explicit --adapter, not the fallback) -- a pi/opencode
            // root's own children should run the same harness unless told
            // otherwise. `ctx.config.defaults.adapter` only still matters for
            // a bare-shell root (`parent.adapter == "shell"`, nothing real to
            // inherit), where it's the only sensible fallback left.
            let adapter_name = adapter.unwrap_or_else(|| {
                if parent.adapter == "shell" {
                    ctx.config.defaults.adapter.clone()
                } else {
                    parent.adapter.clone()
                }
            });
            let req = SpawnRequest {
                role: AgentRole::Child,
                parent_id: Some(parent_id),
                task,
                name,
                adapter_name,
                cwd,
                repo: parent.repo,
                branch: None,
                root_adapter: None,
            };

            match spawn_agent(&ctx.registry, &ctx.sessions, &ctx.socket, &ctx.config, req) {
                Ok(result) => Response::ok(Some(OkBody::Registered {
                    agent_id: result.id,
                    branch: result.branch,
                })),
                Err(e) => Response::err(e.to_string()),
            }
        }

        Request::TuiSpawnChild { parent_id, name } => {
            let name = name.trim().to_string();
            if name.is_empty() {
                return Response::err("child name cannot be empty");
            }
            let parent = match spawn_parent(ctx, &parent_id) {
                Ok(parent) => parent,
                Err(response) => return response,
            };
            if parent.adapter == "shell" {
                return Response::err("parent has no harness configuration to inherit");
            }
            let req = SpawnRequest {
                role: AgentRole::Child,
                parent_id: Some(parent_id),
                task: String::new(),
                name: Some(name),
                adapter_name: parent.adapter,
                cwd: parent.cwd,
                repo: parent.repo,
                branch: None,
                root_adapter: None,
            };
            match spawn_manual_child(&ctx.registry, &ctx.sessions, &ctx.socket, &ctx.config, req) {
                Ok(result) => Response::ok(Some(OkBody::Registered {
                    agent_id: result.id,
                    branch: result.branch,
                })),
                Err(e) => Response::err(e.to_string()),
            }
        }

        Request::Drop { agent_id, recursive } => {
            match drop_agent(&ctx.registry, &ctx.sessions, &agent_id, recursive, false) {
                Ok(()) => Response::ok(None),
                Err(e) => Response::err(e.to_string()),
            }
        }

        // The TUI's own drop keybind — the one caller allowed to drop a root
        // (`allow_root: true`). See `Request::TuiDrop`'s doc comment for why
        // this is a distinct request rather than a flag on `Drop`.
        Request::TuiDrop { agent_id, recursive } => {
            match drop_agent(&ctx.registry, &ctx.sessions, &agent_id, recursive, true) {
                Ok(()) => Response::ok(None),
                Err(e) => Response::err(e.to_string()),
            }
        }

        // These only make sense inside a stateful attach connection — the
        // server's `handle_conn` intercepts `Attach` before a request ever
        // reaches `dispatch`, and switches to a dedicated event-stream loop
        // that owns `Watch`/`Unwatch`/`Write`/`Resize`/`Scroll`/`ScrollToBottom`
        // from then on. Reaching here means one arrived over an ordinary
        // one-shot connection instead.
        Request::Attach
        | Request::Watch { .. }
        | Request::Unwatch
        | Request::Write { .. }
        | Request::Resize { .. }
        | Request::Scroll { .. }
        | Request::ScrollToBottom => {
            Response::err("this request requires an attach connection (Request::Attach first)")
        }

        // `overseer shutdown` (DAEMON.md Task 4): recursive-drops every root
        // (children first, via `drop_agent`'s existing postorder), announces
        // the shutdown to attached clients, then asks `ipc::server::run`'s
        // accept loop to stop. The process itself exits by `main` simply
        // returning once that loop does — no `std::process::exit` — so this
        // request's own response (written by the caller *after* `dispatch`
        // returns) is never raced against the runtime tearing down.
        Request::Shutdown => {
            let root_ids: Vec<AgentId> =
                ctx.registry.with_tree(|t| t.roots.iter().map(|r| r.id.clone()).collect());
            for root_id in root_ids {
                // Best-effort: a root could in principle already be gone by
                // the time we get to it (e.g. a racing `drop` from another
                // connection) — that's not a shutdown failure.
                let _ = drop_agent(&ctx.registry, &ctx.sessions, &root_id, true, true);
            }
            ctx.registry.announce_shutdown();
            Response::ok(None)
        }
    }
}

fn spawn_parent(ctx: &AppCtx, parent_id: &AgentId) -> Result<crate::ipc::protocol::AgentDto, Response> {
    let Some(parent) = ctx.registry.get(parent_id) else {
        return Err(Response::err(format!("unknown agent: {}", parent_id.short())));
    };
    let Some((parent_depth, child_count)) = ctx.registry.spawn_metrics(parent_id) else {
        return Err(Response::err(format!("unknown agent: {}", parent_id.short())));
    };
    if parent_depth >= 3 {
        return Err(Response::err("agents at max depth cannot spawn further agents"));
    }
    if child_count >= ctx.config.defaults.max_children {
        return Err(Response::err(format!(
            "agent reached max_children cap of {}",
            ctx.config.defaults.max_children
        )));
    }
    Ok(parent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentId, AgentStatus};
    use crate::git::GitClient;
    use crate::ipc::protocol::{OkBody, Request};
    use crate::session::SessionManager;
    use std::path::PathBuf;

    fn make_ctx() -> AppCtx {
        make_ctx_with_git(GitClient::dry_run())
    }

    fn make_ctx_with_git(git: GitClient) -> AppCtx {
        AppCtx {
            registry: Arc::new(AgentRegistry::new()),
            sessions: Arc::new(SessionManager::dry_run()),
            socket: PathBuf::from("/tmp/test.sock"),
            git: Arc::new(git),
            config: Arc::new(crate::config::Config::default()),
            watch_sessions: false,
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    #[test]
    fn dispatch_status_unknown_agent_is_error() {
        let ctx = make_ctx();
        let req = Request::Status {
            agent_id: AgentId::new(),
            status: AgentStatus::Done,
            message: None,
            context_pct: None,
            adapter: None,
            pushed_at: std::time::SystemTime::now(),
        };
        let resp = dispatch(&ctx, req);
        assert!(!resp.ok);
    }

    #[test]
    fn dispatch_status_known_agent_succeeds() {
        let ctx = make_ctx();
        let agent_id = start_root(&ctx);

        let status_req = Request::Status {
            agent_id: agent_id.clone(),
            status: AgentStatus::Blocked,
            message: None,
            context_pct: Some(17),
            adapter: None,
            pushed_at: std::time::SystemTime::now(),
        };
        let resp = dispatch(&ctx, status_req);
        assert!(resp.ok);
        assert!(resp.data.is_none());
        assert_eq!(ctx.registry.get(&agent_id).unwrap().context_pct, Some(17));
    }

    #[test]
    fn dispatch_list_returns_agents() {
        let ctx = make_ctx();
        start_root(&ctx);
        let resp = dispatch(&ctx, Request::List);
        assert!(resp.ok);
        assert!(matches!(resp.data, Some(OkBody::Agents { agents }) if agents.len() == 1));
    }

    #[test]
    fn dispatch_agent_unknown_id_is_error() {
        let ctx = make_ctx();
        let resp = dispatch(&ctx, Request::Agent { agent_id: AgentId::new() });
        assert!(!resp.ok);
    }

    #[test]
    fn dispatch_agent_known_id_returns_dto() {
        let ctx = make_ctx();
        let root_id = start_root(&ctx);
        // Children are named after their task text, so spawn one to control
        // the name (Start's dry-run GitClient always names the root "test-repo").
        let spawn_resp = dispatch(&ctx, Request::Spawn {
            parent_id: root_id,
            task: "my-agent".to_string(),
            name: None,
            adapter: Some("claude".to_string()),
            cwd: PathBuf::from("/tmp"),
        });
        let agent_id = match spawn_resp.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            other => panic!("expected Registered, got {other:?}"),
        };

        let resp = dispatch(&ctx, Request::Agent { agent_id });
        assert!(resp.ok);
        assert!(matches!(resp.data, Some(OkBody::Agent { agent }) if agent.name == "my-agent"));
    }

    #[test]
    fn dispatch_start_registers_root_and_returns_agent_id() {
        let ctx = make_ctx();
        let resp = dispatch(&ctx, Request::Start { cwd: None, adapter: None });
        assert!(resp.ok, "Start failed: {:?}", resp.error);
        let (agent_id, branch) = match resp.data {
            Some(OkBody::Registered { agent_id, branch }) => (agent_id, branch),
            other => panic!("expected Registered, got {other:?}"),
        };
        // GitClient::dry_run returns "test-branch"
        assert_eq!(branch, "test-branch");

        // Agent is visible in the registry, named and adapter-labeled per the
        // bare-shell root spawn — no task text is ever passed in.
        let list_resp = dispatch(&ctx, Request::List);
        let agents = match list_resp.data {
            Some(OkBody::Agents { agents }) => agents,
            _ => panic!("expected Agents"),
        };
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, agent_id);
        // GitClient::dry_run returns "test-repo" — the root's name.
        assert_eq!(agents[0].name, "test-repo");
        assert_eq!(agents[0].adapter, "shell");
        assert_eq!(agents[0].status, AgentStatus::Idle);
    }

    // ── Start with an adapter (the `n` picker's second step) ──────────────────

    #[test]
    fn dispatch_start_with_adapter_launches_it_directly_instead_of_a_bare_shell() {
        let ctx = make_ctx();
        let resp = dispatch(&ctx, Request::Start { cwd: None, adapter: Some("claude".to_string()) });
        assert!(resp.ok, "Start with an adapter failed: {:?}", resp.error);
        let agent_id = match resp.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            other => panic!("expected Registered, got {other:?}"),
        };

        let dto = match dispatch(&ctx, Request::Agent { agent_id }).data {
            Some(OkBody::Agent { agent }) => agent,
            other => panic!("expected Agent, got {other:?}"),
        };
        // Adapter label is the chosen adapter, not "shell" — but the PTY
        // child launched is a live shell with the harness command typed
        // into it (ROOT-SHELL-FALLBACK), so status is Idle, same as a
        // bare-shell root's starting state.
        assert_eq!(dto.adapter, "claude");
        assert_eq!(dto.status, AgentStatus::Idle);
        // Still named after the repo, same as the bare-shell root.
        assert_eq!(dto.name, "test-repo");
    }

    #[test]
    fn dispatch_start_with_unknown_adapter_errors_and_registers_nothing() {
        let ctx = make_ctx();
        let resp = dispatch(&ctx, Request::Start { cwd: None, adapter: Some("nonexistent".to_string()) });
        assert!(!resp.ok);
        assert!(resp.error.as_deref().unwrap_or("").contains("unknown adapter"));

        let list_resp = dispatch(&ctx, Request::List);
        assert!(matches!(list_resp.data, Some(OkBody::Agents { agents }) if agents.is_empty()));
    }

    #[test]
    fn dispatch_start_nonexistent_cwd_is_error_and_registers_nothing() {
        // A typo'd path must still hard-fail -- it must not silently register
        // a phantom root under a directory-basename name either.
        let dir = std::env::temp_dir().join(format!("overseer-test-missing-{}", AgentId::new()));

        let ctx = make_ctx_with_git(GitClient::new());
        let resp = dispatch(&ctx, Request::Start { cwd: Some(dir.clone()), adapter: None });

        assert!(!resp.ok, "Start on a nonexistent cwd must fail, got: {:?}", resp.data);
        let error = resp.error.unwrap_or_default();
        assert!(error.contains("not a directory"), "unexpected error: {error}");

        // No root should have been registered for the rejected cwd.
        let list_resp = dispatch(&ctx, Request::List);
        match list_resp.data {
            Some(OkBody::Agents { agents }) => {
                assert!(agents.is_empty(), "expected no registered agents, got: {agents:?}")
            }
            other => panic!("expected Agents, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_start_non_git_dir_registers_with_dir_name_and_empty_branch() {
        // Real (non-dry-run) GitClient against a freshly created, non-git
        // temp directory: this must now succeed, honestly, under the
        // directory's own basename and an explicit empty branch -- not a
        // bogus "unknown"/"main" pair, and not rejected outright either.
        let dir = std::env::temp_dir().join(format!("overseer-test-non-git-{}", AgentId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let expected_name = dir.file_name().unwrap().to_string_lossy().to_string();

        let ctx = make_ctx_with_git(GitClient::new());
        let resp = dispatch(&ctx, Request::Start { cwd: Some(dir.clone()), adapter: None });

        std::fs::remove_dir_all(&dir).ok();

        assert!(resp.ok, "Start on a non-git dir should succeed, got: {:?}", resp.error);
        let (agent_id, branch) = match resp.data {
            Some(OkBody::Registered { agent_id, branch }) => (agent_id, branch),
            other => panic!("expected Registered, got {other:?}"),
        };
        assert_eq!(branch, "", "non-git root must get an explicit empty branch, not a faked one");

        let list_resp = dispatch(&ctx, Request::List);
        match list_resp.data {
            Some(OkBody::Agents { agents }) => {
                assert_eq!(agents.len(), 1);
                assert_eq!(agents[0].id, agent_id);
                assert_eq!(agents[0].name, expected_name);
                assert_eq!(agents[0].repo, expected_name);
            }
            other => panic!("expected Agents, got {other:?}"),
        }
    }

    fn start_root(ctx: &AppCtx) -> AgentId {
        let resp = dispatch(ctx, Request::Start { cwd: None, adapter: None });
        match resp.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            other => panic!("expected Registered, got {other:?}"),
        }
    }

    fn spawn_child(ctx: &AppCtx, parent_id: AgentId, task: &str) -> Response {
        dispatch(ctx, Request::Spawn {
            parent_id,
            task: task.to_string(),
            name: None,
            adapter: Some("claude".to_string()),
            cwd: PathBuf::from("/tmp"),
        })
    }

    fn registered_id(response: Response) -> AgentId {
        match response.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            other => panic!("expected Registered, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_spawn_under_root_succeeds() {
        let ctx = make_ctx();
        let root_id = start_root(&ctx);

        let resp = dispatch(&ctx, Request::Spawn {
            parent_id: root_id.clone(),
            task: "write tests".to_string(),
            name: None,
            adapter: Some("claude".to_string()),
            cwd: PathBuf::from("/tmp"),
        });
        assert!(resp.ok, "Spawn failed: {:?}", resp.error);
        assert!(matches!(resp.data, Some(OkBody::Registered { branch, .. })
            if branch.starts_with("overseer/")));

        let list_resp = dispatch(&ctx, Request::List);
        let agents = match list_resp.data {
            Some(OkBody::Agents { agents }) => agents,
            _ => panic!("expected Agents"),
        };
        assert_eq!(agents.len(), 2);
    }

    #[test]
    fn dispatch_tui_spawn_child_inherits_parent_and_waits_idle() {
        let ctx = make_ctx();
        let start = dispatch(
            &ctx,
            Request::Start { cwd: Some(PathBuf::from("/tmp")), adapter: Some("claude".to_string()) },
        );
        let root_id = registered_id(start);
        let child_id = registered_id(dispatch(
            &ctx,
            Request::TuiSpawnChild { parent_id: root_id, name: "  manual-review  ".to_string() },
        ));
        let child = match dispatch(&ctx, Request::Agent { agent_id: child_id }).data {
            Some(OkBody::Agent { agent }) => agent,
            other => panic!("expected Agent, got {other:?}"),
        };
        assert_eq!(child.name, "manual-review");
        assert_eq!(child.adapter, "claude");
        assert_eq!(child.cwd, PathBuf::from("/tmp"));
        assert_eq!(child.status, AgentStatus::Idle);
    }

    #[test]
    fn dispatch_tui_spawn_child_rejects_blank_name_and_shell_parent() {
        let ctx = make_ctx();
        let root_id = start_root(&ctx);
        let blank = dispatch(
            &ctx,
            Request::TuiSpawnChild { parent_id: root_id.clone(), name: "   ".to_string() },
        );
        assert!(!blank.ok);
        assert!(blank.error.as_deref().unwrap_or("").contains("name"));
        let shell = dispatch(
            &ctx,
            Request::TuiSpawnChild { parent_id: root_id, name: "manual".to_string() },
        );
        assert!(!shell.ok);
        assert!(shell.error.as_deref().unwrap_or("").contains("no harness"));
    }

    // ── child --name ──────────────────────────────────────────────────────────

    #[test]
    fn dispatch_spawn_with_name_registers_that_name_not_the_task() {
        let ctx = make_ctx();
        let root_id = start_root(&ctx);
        let resp = dispatch(&ctx, Request::Spawn {
            parent_id: root_id,
            task: "write unit tests for the login flow".to_string(),
            name: Some("login-tests".to_string()),
            adapter: Some("claude".to_string()),
            cwd: PathBuf::from("/tmp"),
        });
        let agent_id = match resp.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            other => panic!("expected Registered, got {other:?}"),
        };
        let dto = match dispatch(&ctx, Request::Agent { agent_id }).data {
            Some(OkBody::Agent { agent }) => agent,
            other => panic!("expected Agent, got {other:?}"),
        };
        assert_eq!(dto.name, "login-tests");
    }

    #[test]
    fn dispatch_spawn_with_blank_name_falls_back_to_task() {
        let ctx = make_ctx();
        let root_id = start_root(&ctx);
        let resp = dispatch(&ctx, Request::Spawn {
            parent_id: root_id,
            task: "fallback task".to_string(),
            name: Some("   ".to_string()),
            adapter: Some("claude".to_string()),
            cwd: PathBuf::from("/tmp"),
        });
        let agent_id = match resp.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            other => panic!("expected Registered, got {other:?}"),
        };
        let dto = match dispatch(&ctx, Request::Agent { agent_id }).data {
            Some(OkBody::Agent { agent }) => agent,
            other => panic!("expected Agent, got {other:?}"),
        };
        assert_eq!(dto.name, "fallback task");
    }

    #[test]
    fn dispatch_start_ignores_any_supplied_name_and_uses_the_repo() {
        // Request::Start has no `name` field at all — this asserts the
        // invariant a different way: the root is always named after the
        // repo, never influenced by anything spawn-shaped.
        let ctx = make_ctx();
        let root_id = start_root(&ctx);
        let dto = match dispatch(&ctx, Request::Agent { agent_id: root_id }).data {
            Some(OkBody::Agent { agent }) => agent,
            other => panic!("expected Agent, got {other:?}"),
        };
        assert_eq!(dto.name, "test-repo"); // GitClient::dry_run
    }

    #[test]
    fn dispatch_spawn_allows_depth_three_and_rejects_depth_four() {
        let ctx = make_ctx();
        let root_id = start_root(&ctx);
        let child_id = registered_id(spawn_child(&ctx, root_id, "child task"));
        let grandchild_id = registered_id(spawn_child(&ctx, child_id, "grandchild task"));

        assert_eq!(ctx.registry.spawn_metrics(&grandchild_id).map(|m| m.0), Some(3));
        let resp = spawn_child(&ctx, grandchild_id, "too deep");
        assert!(!resp.ok);
        assert!(resp.error.as_deref().unwrap_or("").contains("max depth"));
        assert_eq!(ctx.registry.snapshot().len(), 3);
    }

    #[test]
    fn dispatch_spawn_enforces_per_parent_child_cap() {
        let mut ctx = make_ctx();
        Arc::get_mut(&mut ctx.config).unwrap().defaults.max_children = 2;
        let root_id = start_root(&ctx);

        assert!(spawn_child(&ctx, root_id.clone(), "one").ok);
        assert!(spawn_child(&ctx, root_id.clone(), "two").ok);
        let resp = spawn_child(&ctx, root_id, "three");

        assert!(!resp.ok);
        assert!(resp.error.as_deref().unwrap_or("").contains("max_children cap of 2"));
        assert_eq!(ctx.registry.snapshot().len(), 3);
    }

    #[test]
    fn dispatch_spawn_rejects_a_task_over_the_size_cap() {
        let ctx = make_ctx();
        let root_id = start_root(&ctx);
        let resp = dispatch(&ctx, Request::Spawn {
            parent_id: root_id,
            task: "x".repeat(MAX_SPAWN_TASK_BYTES + 1),
            name: None,
            adapter: Some("claude".to_string()),
            cwd: PathBuf::from("/tmp"),
        });
        assert!(!resp.ok);
        assert!(resp.error.as_deref().unwrap_or("").contains("exceeds max size"));

        // Rejected before touching the registry -- no half-registered agent.
        let list_resp = dispatch(&ctx, Request::List);
        assert!(matches!(list_resp.data, Some(OkBody::Agents { agents }) if agents.len() == 1));
    }

    #[test]
    fn dispatch_spawn_unknown_parent_is_error() {
        let ctx = make_ctx();
        let resp = dispatch(&ctx, Request::Spawn {
            parent_id: AgentId::new(),
            task: "task".to_string(),
            name: None,
            adapter: Some("claude".to_string()),
            cwd: PathBuf::from("/tmp"),
        });
        assert!(!resp.ok);
        assert!(resp.error.as_deref().unwrap_or("").contains("unknown agent"));
    }

    #[test]
    fn dispatch_drop_root_is_rejected() {
        let ctx = make_ctx();
        let root_id = start_root(&ctx);
        let resp = dispatch(&ctx, Request::Drop { agent_id: root_id, recursive: false });
        assert!(!resp.ok);

        let list_resp = dispatch(&ctx, Request::List);
        assert!(matches!(list_resp.data, Some(OkBody::Agents { agents }) if agents.len() == 1));
    }

    #[test]
    fn dispatch_drop_child_succeeds() {
        let ctx = make_ctx();
        let root_id = start_root(&ctx);
        let spawn_resp = dispatch(&ctx, Request::Spawn {
            parent_id: root_id,
            task: "child task".to_string(),
            name: None,
            adapter: Some("claude".to_string()),
            cwd: PathBuf::from("/tmp"),
        });
        let child_id = match spawn_resp.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            other => panic!("expected Registered, got {other:?}"),
        };

        let resp = dispatch(&ctx, Request::Drop { agent_id: child_id, recursive: false });
        assert!(resp.ok, "Drop failed: {:?}", resp.error);

        let list_resp = dispatch(&ctx, Request::List);
        assert!(matches!(list_resp.data, Some(OkBody::Agents { agents }) if agents.len() == 1));
    }

    // ── attach-only requests reaching the one-shot path ──────────────────────

    #[test]
    fn dispatch_rejects_attach_only_requests_outside_an_attach_connection() {
        let ctx = make_ctx();
        for req in [
            Request::Attach,
            Request::Watch { agent_id: AgentId::new() },
            Request::Unwatch,
            Request::Write { agent_id: AgentId::new(), data: "x".to_string() },
            Request::Resize { cols: 80, lines: 24 },
            Request::Scroll { delta: 5 },
            Request::ScrollToBottom,
        ] {
            let resp = dispatch(&ctx, req);
            assert!(!resp.ok, "attach-only request must not succeed over a one-shot connection");
            assert!(resp.error.as_deref().unwrap_or("").contains("attach connection"));
        }
    }

    // ── shutdown ──────────────────────────────────────────────────────────────

    #[test]
    fn dispatch_shutdown_drops_every_root_and_its_children() {
        let ctx = make_ctx();
        let root_a = start_root(&ctx);
        dispatch(&ctx, Request::Spawn {
            parent_id: root_a.clone(),
            task: "child-a".to_string(),
            name: None,
            adapter: Some("claude".to_string()),
            cwd: PathBuf::from("/tmp"),
        });
        let _root_b = start_root(&ctx);

        let resp = dispatch(&ctx, Request::Shutdown);
        assert!(resp.ok, "Shutdown failed: {:?}", resp.error);

        let list_resp = dispatch(&ctx, Request::List);
        assert!(matches!(list_resp.data, Some(OkBody::Agents { agents }) if agents.is_empty()));
    }

    #[test]
    fn dispatch_shutdown_on_an_empty_registry_still_succeeds() {
        let ctx = make_ctx();
        let resp = dispatch(&ctx, Request::Shutdown);
        assert!(resp.ok, "Shutdown on an empty tree must still succeed: {:?}", resp.error);
    }

    #[test]
    fn dispatch_shutdown_broadcasts_removed_for_each_agent_then_shutdown() {
        let ctx = make_ctx();
        let root_id = start_root(&ctx);
        let mut rx = ctx.registry.subscribe();

        dispatch(&ctx, Request::Shutdown);

        match rx.try_recv().unwrap() {
            crate::agent::RegistryEvent::Removed { agent_id } => assert_eq!(agent_id, root_id),
            other => panic!("expected Removed, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            crate::agent::RegistryEvent::Shutdown => {}
            other => panic!("expected Shutdown, got {other:?}"),
        }
    }
}
