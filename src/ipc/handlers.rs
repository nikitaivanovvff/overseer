use crate::agent::{AgentRegistry, RegisterArgs};
use crate::ipc::protocol::{OkBody, Request, Response};

/// Dispatches a parsed request to the registry and returns a wire response.
/// No socket I/O occurs here — this is the unit-testable seam.
pub fn dispatch(reg: &AgentRegistry, req: Request) -> Response {
    match req {
        Request::Register { id, name, role, parent_id, adapter, repo } => {
            let args = RegisterArgs {
                id,
                name,
                role,
                parent_id,
                adapter: adapter.unwrap_or_else(|| "claude".to_string()),
                repo: repo.unwrap_or_else(|| "overseer".to_string()),
            };
            match reg.register(args) {
                Ok(result) => Response::ok(Some(OkBody::Registered {
                    agent_id: result.id,
                    branch: result.branch,
                })),
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::Status { agent_id, status, message } => {
            match reg.set_status(&agent_id, status, message) {
                Ok(()) => Response::ok(None),
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::List => Response::ok(Some(OkBody::Agents { agents: reg.snapshot() })),
        Request::Agent { agent_id } => match reg.get(&agent_id) {
            Some(agent) => Response::ok(Some(OkBody::Agent { agent })),
            None => Response::err(format!("unknown agent: {}", agent_id.short())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentId, AgentRole, AgentStatus};
    use crate::ipc::protocol::{OkBody, Request};

    fn make_registry() -> AgentRegistry {
        AgentRegistry::new()
    }

    #[test]
    fn dispatch_register_root_succeeds() {
        let reg = make_registry();
        let req = Request::Register {
            id: None,
            name: "my-task".to_string(),
            role: AgentRole::Root,
            parent_id: None,
            adapter: None,
            repo: None,
        };
        let resp = dispatch(&reg, req);
        assert!(resp.ok);
        assert!(matches!(resp.data, Some(OkBody::Registered { branch, .. }) if branch == "main"));
    }

    #[test]
    fn dispatch_register_child_unknown_parent_is_error() {
        let reg = make_registry();
        let req = Request::Register {
            id: None,
            name: "child".to_string(),
            role: AgentRole::Child,
            parent_id: Some(AgentId::new()),
            adapter: None,
            repo: None,
        };
        let resp = dispatch(&reg, req);
        assert!(!resp.ok);
        assert!(resp.error.is_some());
    }

    #[test]
    fn dispatch_status_unknown_agent_is_error() {
        let reg = make_registry();
        let req = Request::Status {
            agent_id: AgentId::new(),
            status: AgentStatus::Done,
            message: None,
        };
        let resp = dispatch(&reg, req);
        assert!(!resp.ok);
    }

    #[test]
    fn dispatch_status_known_agent_succeeds() {
        let reg = make_registry();
        let register_req = Request::Register {
            id: None,
            name: "agent".to_string(),
            role: AgentRole::Root,
            parent_id: None,
            adapter: None,
            repo: None,
        };
        let register_resp = dispatch(&reg, register_req);
        let agent_id = match register_resp.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            _ => panic!("expected Registered"),
        };

        let status_req = Request::Status {
            agent_id,
            status: AgentStatus::Waiting,
            message: None,
        };
        let resp = dispatch(&reg, status_req);
        assert!(resp.ok);
        assert!(resp.data.is_none());
    }

    #[test]
    fn dispatch_list_returns_agents() {
        let reg = make_registry();
        dispatch(&reg, Request::Register {
            id: None, name: "a".to_string(), role: AgentRole::Root,
            parent_id: None, adapter: None, repo: None,
        });
        let resp = dispatch(&reg, Request::List);
        assert!(resp.ok);
        assert!(matches!(resp.data, Some(OkBody::Agents { agents }) if agents.len() == 1));
    }

    #[test]
    fn dispatch_agent_unknown_id_is_error() {
        let reg = make_registry();
        let resp = dispatch(&reg, Request::Agent { agent_id: AgentId::new() });
        assert!(!resp.ok);
    }

    #[test]
    fn dispatch_agent_known_id_returns_dto() {
        let reg = make_registry();
        let register_resp = dispatch(&reg, Request::Register {
            id: None, name: "my-agent".to_string(), role: AgentRole::Root,
            parent_id: None, adapter: None, repo: None,
        });
        let agent_id = match register_resp.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            _ => panic!("expected Registered"),
        };

        let resp = dispatch(&reg, Request::Agent { agent_id });
        assert!(resp.ok);
        assert!(matches!(resp.data, Some(OkBody::Agent { agent }) if agent.name == "my-agent"));
    }
}
