use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::drop::drop_agent;
use crate::agent::spawn::{spawn_agent, SpawnRequest};
use crate::agent::{AgentId, AgentRegistry, AgentRole};
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
        Request::Status { agent_id, status, message, context_pct, adapter } => {
            match ctx.registry.set_status(&agent_id, status, message, context_pct, adapter) {
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
                name: None,                  // ignored for Root — always named after the repo
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

        Request::Spawn { parent_id, task, name, adapter, cwd } => {
            let Some(parent) = ctx.registry.get(&parent_id) else {
                return Response::err(format!("unknown agent: {}", parent_id.short()));
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
            // The one and only "no grandchildren" check (AGENTS.md) — not duplicated
            // anywhere else, including the TUI, which routes through this same arm.
            if parent.role == AgentRole::Child {
                return Response::err("children cannot spawn further agents".to_string());
            }

            let req = SpawnRequest {
                role: AgentRole::Child,
                parent_id: Some(parent_id),
                task,
                name,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentId, AgentStatus};
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
    fn dispatch_spawn_under_child_is_rejected() {
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

        // A child trying to spawn its own child — the one "no grandchildren" check.
        let resp = dispatch(&ctx, Request::Spawn {
            parent_id: child_id,
            task: "grandchild task".to_string(),
            name: None,
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
