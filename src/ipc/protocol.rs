//! Newline-delimited JSON wire protocol.
//!
//! One request line → one response line. Examples:
//!   status:   {"cmd":"status","agent_id":"<uuid>","status":"blocked","message":null,"context_pct":62}
//!   list:     {"cmd":"list"}
//!   agent:    {"cmd":"agent","agent_id":"<uuid>"}
//!
//!   ok+data:  {"ok":true,"data":{"agent_id":"<uuid>","branch":"main"}}
//!   ok:       {"ok":true}
//!   error:    {"ok":false,"error":"unknown parent: 00000000"}

use serde::{Deserialize, Serialize};

use crate::agent::{AgentId, AgentNode, AgentRole, AgentStatus};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status {
        agent_id: AgentId,
        status: AgentStatus,
        message: Option<String>,
        /// From `--from-hook` transcript parsing. `None` leaves the node's
        /// existing value untouched — most status pushes don't carry one.
        #[serde(default)]
        context_pct: Option<u8>,
    },
    List,
    Agent {
        agent_id: AgentId,
    },
    /// Server-side launch: register a root agent and start a bare shell for it in
    /// its own PTY, in `cwd` (defaults to the server's own cwd). No adapter is
    /// launched — the user runs their own agent inside it whenever ready.
    Start {
        cwd: Option<std::path::PathBuf>,
    },
    /// Register + launch a child of `parent_id`. Rejected if the parent is itself a
    /// child (flat tree: roots + children only — enforced here, and only here).
    /// `cwd` is always supplied by the caller (agent CLI or TUI) — the server never
    /// falls back to its own working directory for a child.
    Spawn {
        parent_id: AgentId,
        task: String,
        adapter: Option<String>,
        cwd: std::path::PathBuf,
    },
    /// Kill the agent's PTY and deregister it (+ its subtree if `recursive`).
    /// Root agents can only be dropped through the TUI, not this command.
    Drop {
        agent_id: AgentId,
        recursive: bool,
    },
    /// The TUI's own drop keybind (`d`/`D`), the one path allowed to drop a
    /// root (AGENTS.md "root agents cannot be dropped via IPC — only via the
    /// TUI"). Deliberately a distinct request from `Drop` rather than a
    /// caller-supplied flag on it: `overseer drop`/an agent's own CLI calls
    /// only ever construct `Drop`, so the restriction can't be opted out of
    /// from that side. Not a security boundary (this is a local, single-user
    /// socket) — a safety rail against a script or a supervising agent
    /// accidentally killing a whole tree it doesn't own.
    TuiDrop {
        agent_id: AgentId,
        recursive: bool,
    },
    /// Upgrades this connection to a long-lived event stream (DAEMON.md "Attach
    /// protocol") — the daemon replies with an initial `AttachEvent::Snapshot`,
    /// then pushes registry/output events until the connection closes. Once sent,
    /// the connection speaks `AttachEvent` outward and only `Watch`/`Unwatch`/
    /// `Write`/`Resize`/`Shutdown` inward — never a one-shot `Response`.
    Attach,
    /// Starts (or switches) streaming `agent_id`'s rendered terminal grid as
    /// `AttachEvent::Output` on this attach connection. An immediate snapshot is
    /// sent right away so switching the watched agent feels instant, not gated
    /// on the next redraw.
    Watch {
        agent_id: AgentId,
    },
    /// Stops streaming terminal output on this attach connection.
    Unwatch,
    /// Forwards `data` (raw PTY input, always valid UTF-8 in practice — see
    /// AGENTS.md) to `agent_id`'s session. The input-path counterpart to `Watch`.
    Write {
        agent_id: AgentId,
        data: String,
    },
    /// Resizes every agent's PTY to `(cols, lines)` — one shared rect (AGENTS.md
    /// "every agent shares one PTY size"), not per-agent.
    Resize {
        cols: u16,
        lines: u16,
    },
    /// Recursively drops every root, then exits the daemon process — the kill
    /// switch (`overseer shutdown`).
    Shutdown,
}

/// Server-pushed events on an attach connection (DAEMON.md "Attach protocol").
/// Never solicited by a matching one-shot `Response` — `Snapshot` answers
/// `Request::Attach` itself, everything after is unprompted.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AttachEvent {
    /// Sent once, immediately after `Attach` is accepted.
    Snapshot { agents: Vec<AgentDto> },
    AgentRegistered { agent: AgentDto },
    AgentRemoved { agent_id: AgentId },
    StatusChanged {
        agent_id: AgentId,
        status: AgentStatus,
        message: Option<String>,
        context_pct: Option<u8>,
    },
    /// The watched agent's rendered terminal grid. Sent immediately on `Watch`,
    /// then whenever the terminal has produced new content since the last send
    /// (a dirty-flag poll, not per-byte — see `session::pty`).
    Output { agent_id: AgentId, grid: GridSnapshot },
    /// The daemon is exiting (`overseer shutdown`) — every attached client
    /// should treat this the same as the connection closing.
    Shutdown,
}

/// A rendered terminal color, wire-compatible mirror of `ratatui::style::Color`'s
/// variants (minus its own `Reset`-adjacent aliasing) so the daemon can convert
/// from `alacritty_terminal`'s `AnsiColor` without either side depending on the
/// other's color type. See `session::pty::dto_color` / `ui::term_pane`'s
/// `impl From<ColorDto> for Color`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorDto {
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
    Rgb(u8, u8, u8),
    Indexed(u8),
}

/// One rendered grid cell — the wire counterpart of `ui::term_pane::paint_term`'s
/// per-cell styling, minus the wide-char-spacer bookkeeping (a spacer cell is
/// simply `None` in `GridSnapshot::cells`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellDto {
    pub ch: char,
    pub fg: ColorDto,
    pub bg: ColorDto,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

/// A full rendered snapshot of one agent's terminal, streamed in place of raw
/// PTY bytes (see `session::pty` for why: `alacritty_terminal` 0.26 doesn't
/// expose raw incoming bytes without reimplementing its mio/signalfd event
/// loop). The client paints this directly — no local `Term` needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridSnapshot {
    pub cols: u16,
    pub lines: u16,
    /// Row-major, exactly `cols * lines` entries. `None` marks a blank/spacer
    /// cell.
    pub cells: Vec<Option<CellDto>>,
    /// Cursor position as `(line, column)`, `None` if off-screen/hidden.
    pub cursor: Option<(u16, u16)>,
    /// Whether the terminal wants application-cursor-key encoding for arrow
    /// keys. Without a local `Term`, the client has no other way to know
    /// this — `session::keys::encode_key` needs it to encode correctly.
    pub app_cursor_mode: bool,
    /// Whether the terminal wants pasted text wrapped in bracketed-paste
    /// markers. Same rationale as `app_cursor_mode`.
    pub bracketed_paste_mode: bool,
}

impl From<crate::agent::RegistryEvent> for AttachEvent {
    fn from(event: crate::agent::RegistryEvent) -> Self {
        use crate::agent::RegistryEvent;
        match event {
            RegistryEvent::Registered { agent } => AttachEvent::AgentRegistered { agent },
            RegistryEvent::Removed { agent_id } => AttachEvent::AgentRemoved { agent_id },
            RegistryEvent::StatusChanged { agent_id, status, message, context_pct } => {
                AttachEvent::StatusChanged { agent_id, status, message, context_pct }
            }
            RegistryEvent::Shutdown => AttachEvent::Shutdown,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDto {
    pub id: AgentId,
    pub name: String,
    pub status: AgentStatus,
    pub role: AgentRole,
    pub parent_id: Option<AgentId>,
    pub adapter: String,
    pub repo: String,
    pub branch: String,
    pub cwd: std::path::PathBuf,
    pub context_pct: Option<u8>,
}

impl AgentDto {
    pub fn from_node(node: &AgentNode, parent_id: Option<AgentId>) -> Self {
        Self {
            id: node.id.clone(),
            name: node.name.clone(),
            status: node.status.clone(),
            role: node.role.clone(),
            parent_id,
            adapter: node.adapter.clone(),
            repo: node.repo.clone(),
            branch: node.branch.clone(),
            cwd: node.cwd.clone(),
            context_pct: node.context_pct,
        }
    }
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
        assert_eq!(serde_json::to_string(&AgentStatus::Blocked).unwrap(), "\"blocked\"");
        assert_eq!(serde_json::to_string(&AgentStatus::Idle).unwrap(), "\"idle\"");
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
    fn request_status_round_trip() {
        let id = AgentId::new();
        let req = Request::Status {
            agent_id: id.clone(),
            status: AgentStatus::Done,
            message: None,
            context_pct: Some(42),
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(
            matches!(back, Request::Status { status: AgentStatus::Done, context_pct: Some(42), .. })
        );
    }

    #[test]
    fn request_status_context_pct_defaults_to_none_when_absent() {
        // Older callers (or a hand-written request) may omit context_pct entirely.
        let id = AgentId::new();
        let raw = format!(r#"{{"cmd":"status","agent_id":"{}","status":"idle","message":null}}"#, id.0);
        let req: Request = serde_json::from_str(&raw).unwrap();
        assert!(matches!(req, Request::Status { context_pct: None, .. }));
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

    #[test]
    fn request_start_round_trip() {
        let req = Request::Start { cwd: Some(std::path::PathBuf::from("/tmp/myrepo")) };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"cmd\":\"start\""), "should serialize as 'start'");
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::Start { cwd } if cwd == Some(std::path::PathBuf::from("/tmp/myrepo"))));
    }

    #[test]
    fn request_spawn_round_trip() {
        let parent_id = AgentId::new();
        let req = Request::Spawn {
            parent_id: parent_id.clone(),
            task: "write tests".to_string(),
            adapter: Some("claude".to_string()),
            cwd: std::path::PathBuf::from("/repo"),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"cmd\":\"spawn\""), "should serialize as 'spawn'");
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::Spawn { task, parent_id: p, .. }
            if task == "write tests" && p == parent_id));
    }

    #[test]
    fn request_drop_round_trip() {
        let agent_id = AgentId::new();
        let req = Request::Drop { agent_id: agent_id.clone(), recursive: true };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"cmd\":\"drop\""), "should serialize as 'drop'");
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::Drop { agent_id: id, recursive: true } if id == agent_id));
    }
}
