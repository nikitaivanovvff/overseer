use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::drop::drop_agent;
use crate::agent::spawn::{spawn_agent, SpawnRequest};
use crate::agent::{AgentRegistry, AgentRole, AgentStatus, RegisterArgs};
use crate::config::Config;
use crate::git::GitClient;
use crate::ipc::protocol::{OkBody, Request, Response};
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
}

/// Dispatches a parsed request to the registry and returns a wire response.
/// No socket I/O occurs here — this is the unit-testable seam.
/// Blocking calls (git, session launch) are expected to run inside `spawn_blocking` at the call site.
pub fn dispatch(ctx: &AppCtx, req: Request) -> Response {
    match req {
        Request::Register { id, name, role, parent_id, adapter, repo } => {
            let args = RegisterArgs {
                id,
                name,
                role,
                parent_id,
                adapter: adapter.unwrap_or_else(|| "claude".to_string()),
                repo: repo.unwrap_or_else(|| "overseer".to_string()),
                cwd: PathBuf::from("."),
                branch: None,
                // Request::Register is a defined-but-currently-unused primitive
                // (no hook or caller invokes it today) — Running matches the
                // pre-existing hardcoded behavior it's replacing.
                initial_status: AgentStatus::Running,
            };
            match ctx.registry.register(args) {
                Ok(result) => Response::ok(Some(OkBody::Registered {
                    agent_id: result.id,
                    branch: result.branch,
                })),
                Err(e) => Response::err(e.to_string()),
            }
        }

        Request::Status { agent_id, status, message, context_pct } => {
            match ctx.registry.set_status(&agent_id, status, message, context_pct) {
                Ok(()) => Response::ok(None),
                Err(e) => Response::err(e.to_string()),
            }
        }

        Request::List => Response::ok(Some(OkBody::Agents { agents: ctx.registry.snapshot() })),

        Request::Agent { agent_id } => match ctx.registry.get(&agent_id) {
            Some(agent) => Response::ok(Some(OkBody::Agent { agent })),
            None => Response::err(format!("unknown agent: {}", agent_id.short())),
        },

        Request::Start { cwd } => {
            let cwd = cwd.unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
            });

            let repo = ctx.git.repo_name(&cwd).unwrap_or_else(|_| "unknown".to_string());
            let branch = ctx.git.current_branch(&cwd).unwrap_or_else(|_| "main".to_string());

            let req = SpawnRequest {
                role: AgentRole::Root,
                parent_id: None,
                task: String::new(),         // ignored for Root — no adapter is launched
                adapter_name: String::new(), // ignored for Root — always a bare shell
                cwd,
                repo,
                branch: Some(branch),
            };

            match spawn_agent(&ctx.registry, &ctx.sessions, &ctx.socket, &ctx.config, req) {
                Ok(result) => Response::ok(Some(OkBody::Registered {
                    agent_id: result.id,
                    branch: result.branch,
                })),
                Err(e) => Response::err(e.to_string()),
            }
        }

        Request::Spawn { parent_id, task, adapter, cwd } => {
            let adapter_name = adapter.unwrap_or_else(|| ctx.config.defaults.adapter.clone());

            let Some(parent) = ctx.registry.get(&parent_id) else {
                return Response::err(format!("unknown agent: {}", parent_id.short()));
            };
            // The one and only "no grandchildren" check (AGENTS.md) — not duplicated
            // anywhere else, including the TUI, which routes through this same arm.
            if parent.role == AgentRole::Child {
                return Response::err("children cannot spawn further agents".to_string());
            }

            let req = SpawnRequest {
                role: AgentRole::Child,
                parent_id: Some(parent_id),
                task,
                adapter_name,
                cwd,
                repo: parent.repo,
                branch: None,
            };

            match spawn_agent(&ctx.registry, &ctx.sessions, &ctx.socket, &ctx.config, req) {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentId, AgentRole, AgentStatus};
    use crate::git::GitClient;
    use crate::ipc::protocol::{OkBody, Request};
    use crate::session::SessionManager;
    use std::path::PathBuf;

    fn make_ctx() -> AppCtx {
        AppCtx {
            registry: Arc::new(AgentRegistry::new()),
            sessions: Arc::new(SessionManager::dry_run()),
            socket: PathBuf::from("/tmp/test.sock"),
            git: Arc::new(GitClient::dry_run()),
            config: Arc::new(crate::config::Config::default()),
            watch_sessions: false,
        }
    }

    #[test]
    fn dispatch_register_root_succeeds() {
        let ctx = make_ctx();
        let req = Request::Register {
            id: None,
            name: "my-task".to_string(),
            role: AgentRole::Root,
            parent_id: None,
            adapter: None,
            repo: None,
        };
        let resp = dispatch(&ctx, req);
        assert!(resp.ok);
        assert!(matches!(resp.data, Some(OkBody::Registered { branch, .. }) if branch == "main"));
    }

    #[test]
    fn dispatch_register_child_unknown_parent_is_error() {
        let ctx = make_ctx();
        let req = Request::Register {
            id: None,
            name: "child".to_string(),
            role: AgentRole::Child,
            parent_id: Some(AgentId::new()),
            adapter: None,
            repo: None,
        };
        let resp = dispatch(&ctx, req);
        assert!(!resp.ok);
        assert!(resp.error.is_some());
    }

    #[test]
    fn dispatch_status_unknown_agent_is_error() {
        let ctx = make_ctx();
        let req = Request::Status {
            agent_id: AgentId::new(),
            status: AgentStatus::Done,
            message: None,
            context_pct: None,
        };
        let resp = dispatch(&ctx, req);
        assert!(!resp.ok);
    }

    #[test]
    fn dispatch_status_known_agent_succeeds() {
        let ctx = make_ctx();
        let register_req = Request::Register {
            id: None,
            name: "agent".to_string(),
            role: AgentRole::Root,
            parent_id: None,
            adapter: None,
            repo: None,
        };
        let register_resp = dispatch(&ctx, register_req);
        let agent_id = match register_resp.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            _ => panic!("expected Registered"),
        };

        let status_req = Request::Status {
            agent_id: agent_id.clone(),
            status: AgentStatus::Blocked,
            message: None,
            context_pct: Some(17),
        };
        let resp = dispatch(&ctx, status_req);
        assert!(resp.ok);
        assert!(resp.data.is_none());
        assert_eq!(ctx.registry.get(&agent_id).unwrap().context_pct, Some(17));
    }

    #[test]
    fn dispatch_list_returns_agents() {
        let ctx = make_ctx();
        dispatch(&ctx, Request::Register {
            id: None, name: "a".to_string(), role: AgentRole::Root,
            parent_id: None, adapter: None, repo: None,
        });
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
        let register_resp = dispatch(&ctx, Request::Register {
            id: None, name: "my-agent".to_string(), role: AgentRole::Root,
            parent_id: None, adapter: None, repo: None,
        });
        let agent_id = match register_resp.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            _ => panic!("expected Registered"),
        };

        let resp = dispatch(&ctx, Request::Agent { agent_id });
        assert!(resp.ok);
        assert!(matches!(resp.data, Some(OkBody::Agent { agent }) if agent.name == "my-agent"));
    }

    #[test]
    fn dispatch_start_registers_root_and_returns_agent_id() {
        let ctx = make_ctx();
        let resp = dispatch(&ctx, Request::Start { cwd: None });
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

    fn start_root(ctx: &AppCtx) -> AgentId {
        let resp = dispatch(ctx, Request::Start { cwd: None });
        match resp.data {
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
    fn dispatch_spawn_under_child_is_rejected() {
        let ctx = make_ctx();
        let root_id = start_root(&ctx);
        let spawn_resp = dispatch(&ctx, Request::Spawn {
            parent_id: root_id,
            task: "child task".to_string(),
            adapter: Some("claude".to_string()),
            cwd: PathBuf::from("/tmp"),
        });
        let child_id = match spawn_resp.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            other => panic!("expected Registered, got {other:?}"),
        };

        // A child trying to spawn its own child — the one "no grandchildren" check.
        let resp = dispatch(&ctx, Request::Spawn {
            parent_id: child_id,
            task: "grandchild task".to_string(),
            adapter: Some("claude".to_string()),
            cwd: PathBuf::from("/tmp"),
        });
        assert!(!resp.ok);
        assert!(resp.error.as_deref().unwrap_or("").contains("cannot spawn"));
    }

    #[test]
    fn dispatch_spawn_unknown_parent_is_error() {
        let ctx = make_ctx();
        let resp = dispatch(&ctx, Request::Spawn {
            parent_id: AgentId::new(),
            task: "task".to_string(),
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
}
