use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::adapters::{adapter_for, LaunchContext};
use crate::agent::{AgentRegistry, AgentRole, RegisterArgs};
use crate::git::GitClient;
use crate::ipc::protocol::{OkBody, Request, Response};
use crate::session::TmuxClient;

/// Shared context injected into every IPC handler.
#[derive(Clone)]
pub struct AppCtx {
    pub registry: Arc<AgentRegistry>,
    pub tmux: Arc<TmuxClient>,
    pub socket: PathBuf,
    pub git: Arc<GitClient>,
    /// When true, the server spawns a background task that marks agents whose tmux
    /// session has exited as Error. Set false in mock/test mode.
    pub watch_sessions: bool,
}

/// Dispatches a parsed request to the registry and returns a wire response.
/// No socket I/O occurs here — this is the unit-testable seam.
/// Blocking calls (git, tmux) are expected to run inside `spawn_blocking` at the call site.
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
                branch: None,
            };
            match ctx.registry.register(args) {
                Ok(result) => Response::ok(Some(OkBody::Registered {
                    agent_id: result.id,
                    branch: result.branch,
                })),
                Err(e) => Response::err(e.to_string()),
            }
        }

        Request::Status { agent_id, status, message } => {
            match ctx.registry.set_status(&agent_id, status, message) {
                Ok(()) => Response::ok(None),
                Err(e) => Response::err(e.to_string()),
            }
        }

        Request::List => Response::ok(Some(OkBody::Agents { agents: ctx.registry.snapshot() })),

        Request::Agent { agent_id } => match ctx.registry.get(&agent_id) {
            Some(agent) => Response::ok(Some(OkBody::Agent { agent })),
            None => Response::err(format!("unknown agent: {}", agent_id.short())),
        },

        Request::Start { task, adapter, cwd } => {
            let adapter_name = adapter.as_deref().unwrap_or("claude").to_string();
            let Some(adapter) = adapter_for(&adapter_name) else {
                return Response::err(format!("unknown adapter: {adapter_name}"));
            };

            let cwd = cwd.unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
            });

            let repo = ctx.git.repo_name(&cwd).unwrap_or_else(|_| "unknown".to_string());
            let branch = ctx.git.current_branch(&cwd).unwrap_or_else(|_| "main".to_string());

            let args = RegisterArgs {
                id: None,
                name: task.clone(),
                role: AgentRole::Root,
                parent_id: None,
                adapter: adapter_name,
                repo,
                branch: Some(branch),
            };
            let result = match ctx.registry.register(args) {
                Ok(r) => r,
                Err(e) => return Response::err(e.to_string()),
            };

            let launch_ctx = LaunchContext {
                agent_id: result.id.clone(),
                role: AgentRole::Root,
                parent_id: None,
                socket: ctx.socket.clone(),
                cwd,
                task,
                command: "claude".to_string(),
                extra_args: vec![],
            };

            if let Err(e) = crate::agent::spawn::launch(&launch_ctx, adapter.as_ref(), &ctx.tmux) {
                return Response::err(format!("launch failed: {e}"));
            }

            Response::ok(Some(OkBody::Registered {
                agent_id: result.id,
                branch: result.branch,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentId, AgentRole, AgentStatus};
    use crate::git::GitClient;
    use crate::ipc::protocol::{OkBody, Request};
    use crate::session::TmuxClient;
    use std::path::PathBuf;

    fn make_ctx() -> AppCtx {
        AppCtx {
            registry: Arc::new(AgentRegistry::new()),
            tmux: Arc::new(TmuxClient::dry_run()),
            socket: PathBuf::from("/tmp/test.sock"),
            git: Arc::new(GitClient::dry_run()),
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
            agent_id,
            status: AgentStatus::Waiting,
            message: None,
        };
        let resp = dispatch(&ctx, status_req);
        assert!(resp.ok);
        assert!(resp.data.is_none());
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
        let resp = dispatch(&ctx, Request::Start {
            task: "implement auth".to_string(),
            adapter: Some("claude".to_string()),
            cwd: None,
        });
        assert!(resp.ok, "Start failed: {:?}", resp.error);
        let (agent_id, branch) = match resp.data {
            Some(OkBody::Registered { agent_id, branch }) => (agent_id, branch),
            other => panic!("expected Registered, got {other:?}"),
        };
        // GitClient::dry_run returns "test-branch"
        assert_eq!(branch, "test-branch");

        // Agent is visible in the registry.
        let list_resp = dispatch(&ctx, Request::List);
        let agents = match list_resp.data {
            Some(OkBody::Agents { agents }) => agents,
            _ => panic!("expected Agents"),
        };
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, agent_id);
        assert_eq!(agents[0].name, "implement auth");
    }

    #[test]
    fn dispatch_start_unknown_adapter_is_error() {
        let ctx = make_ctx();
        let resp = dispatch(&ctx, Request::Start {
            task: "task".to_string(),
            adapter: Some("nonexistent".to_string()),
            cwd: None,
        });
        assert!(!resp.ok);
        assert!(resp.error.as_deref().unwrap_or("").contains("unknown adapter"));
    }
}
