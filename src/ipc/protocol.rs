//! Newline-delimited JSON wire protocol.
//!
//! One request line → one response line. Examples:
//!   register: {"cmd":"register","id":null,"name":"my-task","role":"root","parent_id":null,"adapter":"claude","repo":"overseer"}
//!   status:   {"cmd":"status","agent_id":"<uuid>","status":"waiting","message":null}
//!   list:     {"cmd":"list"}
//!   agent:    {"cmd":"agent","agent_id":"<uuid>"}
//!
//!   ok+data:  {"ok":true,"data":{"agent_id":"<uuid>","branch":"main"}}
//!   ok:       {"ok":true}
//!   error:    {"ok":false,"error":"unknown parent: 00000000"}

use serde::{Deserialize, Serialize};

use crate::agent::{AgentId, AgentRole, AgentStatus};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Register {
        id: Option<AgentId>,
        name: String,
        role: AgentRole,
        parent_id: Option<AgentId>,
        adapter: Option<String>,
        repo: Option<String>,
    },
    Status {
        agent_id: AgentId,
        status: AgentStatus,
        message: Option<String>,
    },
    List,
    Agent {
        agent_id: AgentId,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentDto {
    pub id: AgentId,
    pub name: String,
    pub status: AgentStatus,
    pub role: AgentRole,
    pub parent_id: Option<AgentId>,
    pub adapter: String,
    pub repo: String,
    pub branch: String,
    pub context_pct: Option<u8>,
}

/// Response envelope.
///
/// Success with data: `{"ok":true,"data":{...}}`
/// Success no data:   `{"ok":true}`
/// Error:             `{"ok":false,"error":"<message>"}`
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<OkBody>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OkBody {
    Registered { agent_id: AgentId, branch: String },
    Agents { agents: Vec<AgentDto> },
    Agent { agent: AgentDto },
}

impl Response {
    pub fn ok(data: Option<OkBody>) -> Self {
        Self { ok: true, error: None, data }
    }

    pub fn err(error: impl Into<String>) -> Self {
        Self { ok: false, error: Some(error.into()), data: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&AgentStatus::Running).unwrap(), "\"running\"");
        assert_eq!(serde_json::to_string(&AgentStatus::Waiting).unwrap(), "\"waiting\"");
        assert_eq!(serde_json::to_string(&AgentStatus::Spawning).unwrap(), "\"spawning\"");
        assert_eq!(serde_json::to_string(&AgentStatus::Done).unwrap(), "\"done\"");
        assert_eq!(serde_json::to_string(&AgentStatus::Error).unwrap(), "\"error\"");
    }

    #[test]
    fn role_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&AgentRole::Root).unwrap(), "\"root\"");
        assert_eq!(serde_json::to_string(&AgentRole::Child).unwrap(), "\"child\"");
    }

    #[test]
    fn request_list_round_trip() {
        let req = Request::List;
        let s = serde_json::to_string(&req).unwrap();
        assert_eq!(s, r#"{"cmd":"list"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::List));
    }

    #[test]
    fn request_register_round_trip() {
        let req = Request::Register {
            id: None,
            name: "my-task".to_string(),
            role: AgentRole::Root,
            parent_id: None,
            adapter: Some("claude".to_string()),
            repo: Some("overseer".to_string()),
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::Register { name, .. } if name == "my-task"));
    }

    #[test]
    fn request_status_round_trip() {
        let id = AgentId::new();
        let req = Request::Status {
            agent_id: id.clone(),
            status: AgentStatus::Done,
            message: None,
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::Status { status: AgentStatus::Done, .. }));
    }

    #[test]
    fn request_agent_round_trip() {
        let id = AgentId::new();
        let req = Request::Agent { agent_id: id.clone() };
        let s = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::Agent { .. }));
    }

    #[test]
    fn response_ok_no_data_round_trip() {
        let resp = Response::ok(None);
        let s = serde_json::to_string(&resp).unwrap();
        assert_eq!(s, r#"{"ok":true}"#);
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(back.ok);
        assert!(back.data.is_none());
    }

    #[test]
    fn response_ok_registered_round_trip() {
        let id = AgentId::new();
        let resp = Response::ok(Some(OkBody::Registered {
            agent_id: id.clone(),
            branch: "main".to_string(),
        }));
        let s = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(back.ok);
        assert!(matches!(back.data, Some(OkBody::Registered { branch, .. }) if branch == "main"));
    }

    #[test]
    fn response_err_round_trip() {
        let resp = Response::err("unknown agent: abc12345");
        let s = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(!back.ok);
        assert_eq!(back.error.as_deref(), Some("unknown agent: abc12345"));
    }

    #[test]
    fn response_agents_list_round_trip() {
        let resp = Response::ok(Some(OkBody::Agents { agents: vec![] }));
        let s = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(matches!(back.data, Some(OkBody::Agents { agents }) if agents.is_empty()));
    }
}
