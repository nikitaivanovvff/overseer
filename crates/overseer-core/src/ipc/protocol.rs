//! Newline-delimited JSON wire protocol.
//!
//! One request line → one response line. Examples:
//!   status:   {"cmd":"status","agent_id":"<uuid>","status":"blocked","message":null,"context_pct":62,"pushed_at":{"secs_since_epoch":1234,"nanos_since_epoch":0}}
//!   list:     {"cmd":"list"}
//!   agent:    {"cmd":"agent","agent_id":"<uuid>"}
//!
//!   ok+data:  {"ok":true,"data":{"agent_id":"<uuid>","branch":"main"}}
//!   ok:       {"ok":true}
//!   error:    {"ok":false,"error":"unknown parent: 00000000"}
//!
//! # SECURITY: every agent under one daemon fully trusts every other agent
//!
//! `agent_id` is a plain, caller-supplied field on every request below — it
//! is never checked against the identity of the connection sending it,
//! because the wire protocol has no notion of connection identity at all.
//! Concretely, any process holding `OVERSEER_SOCKET` (i.e. any agent this
//! daemon launched, root or child) can:
//! - `Write` raw bytes into **any other agent's** PTY (including the root
//!   shell's), which is a real cross-agent code-execution path, not just a
//!   UI nuisance;
//! - push a `Status` for any `agent_id`, forging the tree a human operator
//!   reads to make trust decisions;
//! - `Drop` any non-root agent regardless of who spawned it, or `Shutdown`
//!   the whole daemon (every session for the user).
//!
//! This is a deliberate, accepted trade-off (SECURITY-AUDIT.md F4), not an
//! oversight: the socket has no `SO_PEERCRED` check and the protocol has no
//! per-agent auth handshake. Do not run mutually-distrusting agents under
//! one daemon — the isolation this tool provides is organizational (a tree
//! you can see and `drop`), not a security sandbox between siblings.

use serde::{Deserialize, Serialize};

use crate::agent::adapters::AdapterCapabilities;
use crate::agent::{AgentId, AgentNode, AgentRole, AgentStatus, Attention, AttentionUpdate};

/// Max size of one `Write.data` payload (SECURITY-AUDIT.md F2). A single
/// keystroke is a few bytes; a large paste is the only realistic case, and
/// 64 KiB is generous for that. Bounds how much a hostile agent can flood a
/// sibling's PTY with in one request.
pub const MAX_WRITE_DATA_BYTES: usize = 64 * 1024;

/// Max size of one `Spawn.task` payload (SECURITY-AUDIT.md F2). `task`
/// becomes a process argv entry — well above any realistic initial prompt,
/// but far below sizes that risk `E2BIG` from the OS after allocation.
pub const MAX_SPAWN_TASK_BYTES: usize = 128 * 1024;
pub const MAX_ATTENTION_MESSAGE_BYTES: usize = 4 * 1024;

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
        /// Authoritative model identifier reported by the harness. `None`
        /// preserves the last known value.
        #[serde(default)]
        model_name: Option<String>,
        #[serde(default)]
        clear_context: bool,
        #[serde(default)]
        attention: Option<AttentionUpdate>,
        /// Self-identifies the pushing session's actual harness. `None`
        /// leaves the node's existing value untouched — only each adapter's
        /// own SessionStart-equivalent install hook passes this, once, so a
        /// bare-shell root (`overseer start` never launches an adapter, so
        /// it's always registered as "shell") stops looking like "shell" the
        /// moment the user actually runs claude/opencode inside it.
        #[serde(default)]
        adapter: Option<String>,
        /// The pushing session's actual current git branch, self-reported —
        /// never guessed or synthesized by the daemon. `None` leaves the
        /// node's existing value untouched. Every `overseer status` push
        /// auto-detects this from the CLI process's own cwd (`git rev-parse
        /// --abbrev-ref HEAD`, mirroring `GitClient::current_branch` but run
        /// from the agent process's side) unless the caller passes an
        /// explicit `--branch` override, so a child's tree row goes from a
        /// synthetic `overseer/<id>` placeholder to its real branch the
        /// moment its own worktree setup causes the next hook fire's cwd to
        /// reflect it.
        #[serde(default)]
        branch: Option<String>,
        /// The pushing session's actual current repo, self-reported the same
        /// way as `branch` — the git repo root's basename when the process's
        /// cwd is inside one (stable across `cd`s within that repo, since it
        /// comes from `git rev-parse --show-toplevel`), else the bare
        /// directory's own basename. `None` leaves the node's existing value
        /// untouched. Only ever sent by the pushing CLI when `$OVERSEER_ROLE
        /// == "root"`, and only ever applied by the registry to a root node
        /// — a workspace's tree label, static since `overseer start`, snaps
        /// to the real repo the moment the first status push (e.g. a
        /// harness's SessionStart-equivalent hook) fires from inside
        /// whatever directory the bare shell was `cd`'d into.
        #[serde(default)]
        repo: Option<String>,
        /// Wall-clock time this push was captured at, client-side, as early
        /// as possible in the `overseer status` process's life (see
        /// `main.rs`) — not daemon-arrival time. Every hook fire is its own
        /// short-lived OS process making its own fresh connection with no
        /// ordering guarantee between connections (`ipc::server` spawns an
        /// independent task per accepted connection), so a push that fired
        /// earlier can still arrive later than one that fired after it.
        /// `AgentRegistry::set_status` uses this to drop a push that's older
        /// than the newest one already applied, instead of last-write-wins
        /// on arrival order (`STATUS-RACE.md`). Defaults to the *daemon's*
        /// receive-time for a caller that omits it, which degenerates to the
        /// old last-write-wins behavior for that single push only.
        #[serde(default = "std::time::SystemTime::now")]
        pushed_at: std::time::SystemTime,
    },
    List,
    Agent {
        agent_id: AgentId,
    },
    /// Server-side launch: register a root agent and start a bare shell for
    /// it in its own PTY, in `cwd` (defaults to the server's own cwd). No
    /// adapter is launched — the user runs their own agent inside it
    /// whenever ready.
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
        /// Short tree-row label, distinct from `task` (which can be a whole
        /// paragraph as the child's initial prompt). Absent or blank falls
        /// back to using `task` verbatim as the name, same as before this
        /// field existed. Display-only — never validated or truncated
        /// server-side (the tree already truncates for rendering).
        #[serde(default)]
        name: Option<String>,
        adapter: Option<String>,
        cwd: std::path::PathBuf,
    },
    /// TUI-only path for a named, taskless child that inherits its parent's
    /// configured harness and waits idle for the human's first prompt.
    TuiSpawnChild {
        parent_id: AgentId,
        name: String,
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
    /// from that side. This guards against *accidental* misuse (a script or
    /// a supervising agent killing a whole tree it doesn't own) — it is not
    /// an authorization boundary between agents; see this module's top-level
    /// SECURITY note (SECURITY-AUDIT.md F4) for what's actually enforced.
    TuiDrop {
        agent_id: AgentId,
        recursive: bool,
    },
    /// Upgrades this connection to a long-lived event stream (DAEMON.md "Attach
    /// protocol") — the daemon replies with an initial `AttachEvent::Snapshot`,
    /// then pushes registry/output events until the connection closes. Once sent,
    /// the connection speaks `AttachEvent` outward and only `Watch`/`Unwatch`/
    /// `Write`/`Resize`/`Scroll`/`ScrollToBottom` inward — never a one-shot
    /// `Response`.
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
    /// Scrolls the currently **watched** agent's terminal history — positive
    /// `delta` moves up (further into scrollback), negative moves down
    /// (toward live). No `agent_id`: scrolling only ever applies to whichever
    /// agent this connection is watching (SCROLLBACK.md). A no-op if nothing
    /// is currently watched.
    Scroll {
        delta: i32,
    },
    /// Jumps the watched agent's terminal back to the live bottom (`G` in the
    /// TUI). Same no-op rule as `Scroll`.
    ScrollToBottom,
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
    /// No `status_secs` here (unlike `AgentDto`) — a client only needs to
    /// know whether this transition is an actual change (to decide whether
    /// to reset its own `status_since` clock and fire a bell/notification),
    /// and it can determine that itself by comparing against the status it
    /// already has stored. Its own `Instant::now()` at that moment is close
    /// enough to the daemon's own reset instant to not matter.
    StatusChanged {
        agent_id: AgentId,
        status: AgentStatus,
        message: Option<String>,
        context_pct: Option<u8>,
        model_name: Option<String>,
        attention: Option<Attention>,
        adapter: String,
        /// The node's current (merged) branch, same "definitive value, not a
        /// delta" posture as `adapter` — a push that didn't self-report one
        /// still broadcasts whatever the node already had.
        branch: String,
        /// The node's current (merged) repo, same "definitive value, not a
        /// delta" posture as `branch` — only ever actually changes for a
        /// root/workspace node (see `AgentRegistry::set_status_update_with_model`).
        repo: String,
        /// The node's current (merged) display name — kept equal to `repo`
        /// for a root node, otherwise the child's own given/task-derived
        /// label, unaffected by this push.
        name: String,
        /// See `AgentNode::session_alive` — the definitive value, applied
        /// directly by `app::apply_event` same as every other field here.
        session_alive: bool,
    },
    /// The watched agent's rendered terminal grid. Sent immediately on `Watch`,
    /// then whenever the terminal has produced new content since the last send
    /// (a content-generation poll, not per-byte — see `session::pty`).
    Output { agent_id: AgentId, grid: GridSnapshot },
    /// The daemon is exiting (`overseer shutdown`) — every attached client
    /// should treat this the same as the connection closing.
    Shutdown,
}

/// A rendered terminal color, wire-compatible mirror of `ratatui::style::Color`'s
/// variants (minus its own `Reset`-adjacent aliasing) so the daemon can convert
/// from the terminal emulator's own color type without either side depending
/// on the other's. See `session::pty::dto_color` / `ui::term_pane::map_dto_color`.
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

/// One rendered grid cell — the wire counterpart of
/// `ui::term_pane::paint_grid_snapshot`'s per-cell styling, minus the
/// wide-char-spacer bookkeeping (a spacer cell is simply `None` in
/// `GridSnapshot::cells`).
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
/// PTY bytes (see `session::pty` for why: the terminal emulator crate doesn't
/// expose raw incoming bytes without reimplementing its mio/signalfd event
/// loop). The client paints this directly — no local terminal state needed.
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
    /// Whether the application requested terminal mouse reports, plus the
    /// negotiated encoding variants needed to forward focused-pane events.
    #[serde(default)]
    pub mouse_reporting_mode: bool,
    #[serde(default)]
    pub sgr_mouse_mode: bool,
    #[serde(default)]
    pub utf8_mouse_mode: bool,
    /// How far this snapshot is scrolled up from the live bottom (`0` = live).
    /// Drives the pane's "[scrolled ↑N — G to follow]" title (SCROLLBACK.md).
    pub display_offset: usize,
}

impl From<crate::agent::RegistryEvent> for AttachEvent {
    fn from(event: crate::agent::RegistryEvent) -> Self {
        use crate::agent::RegistryEvent;
        match event {
            RegistryEvent::Registered { agent } => AttachEvent::AgentRegistered { agent },
            RegistryEvent::Removed { agent_id } => AttachEvent::AgentRemoved { agent_id },
            RegistryEvent::StatusChanged {
                agent_id,
                status,
                message,
                context_pct,
                model_name,
                attention,
                adapter,
                branch,
                repo,
                name,
                session_alive,
            } => AttachEvent::StatusChanged {
                agent_id,
                status,
                message,
                context_pct,
                model_name,
                attention,
                adapter,
                branch,
                repo,
                name,
                session_alive,
            },
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
    pub model_name: Option<String>,
    pub attention: Option<Attention>,
    /// See `AgentNode::session_alive`.
    pub session_alive: bool,
    pub capabilities: Box<AdapterCapabilities>,
    /// How long `status` has held its current value, in whole seconds,
    /// computed at snapshot time (ATTENTION.md) — an age, not the
    /// `Instant` itself, since that has no meaning across the wire. Shown to
    /// the root agent too via `overseer list`/`overseer agent`, which is
    /// what makes "check on long-idle children" actionable.
    pub status_secs: u64,
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
            model_name: node.model_name.clone(),
            attention: node.attention.clone(),
            session_alive: node.session_alive,
            capabilities: Box::new(crate::agent::adapters::capabilities_for(&node.adapter)),
            status_secs: node.status_since.elapsed().as_secs(),
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
    Agent { agent: Box<AgentDto> },
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
    fn capability_support_preserves_unsupported_reason_and_experimental_note() {
        use crate::agent::adapters::CapabilitySupport;
        let unsupported = CapabilitySupport::Unsupported { reason: "no event".to_string() };
        let experimental = CapabilitySupport::Experimental { note: "probe pending".to_string() };
        let unsupported_json = serde_json::to_value(&unsupported).unwrap();
        let experimental_json = serde_json::to_value(&experimental).unwrap();
        assert_eq!(unsupported_json["support"], "unsupported");
        assert_eq!(unsupported_json["reason"], "no event");
        assert_eq!(experimental_json["support"], "experimental");
        assert_eq!(experimental_json["note"], "probe pending");
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
            model_name: Some("anthropic/claude-sonnet-5".to_string()),
            clear_context: false,
            attention: None,
            adapter: None,
            branch: Some("ovsr/auth".to_string()),
            repo: Some("overseer".to_string()),
            pushed_at: std::time::SystemTime::now(),
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(
            matches!(back, Request::Status { status: AgentStatus::Done, context_pct: Some(42), model_name: Some(model), branch: Some(branch), repo: Some(repo), .. }
                if model == "anthropic/claude-sonnet-5" && branch == "ovsr/auth" && repo == "overseer")
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
    fn request_status_branch_defaults_to_none_when_absent() {
        // Older callers (or a hand-written request) may omit branch entirely.
        let id = AgentId::new();
        let raw = format!(r#"{{"cmd":"status","agent_id":"{}","status":"idle","message":null}}"#, id.0);
        let req: Request = serde_json::from_str(&raw).unwrap();
        assert!(matches!(req, Request::Status { branch: None, .. }));
    }

    #[test]
    fn request_status_repo_defaults_to_none_when_absent() {
        // Older callers (or a hand-written request) may omit repo entirely.
        let id = AgentId::new();
        let raw = format!(r#"{{"cmd":"status","agent_id":"{}","status":"idle","message":null}}"#, id.0);
        let req: Request = serde_json::from_str(&raw).unwrap();
        assert!(matches!(req, Request::Status { repo: None, .. }));
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
        assert!(matches!(back, Request::Start { cwd }
            if cwd == Some(std::path::PathBuf::from("/tmp/myrepo"))));
    }

    #[test]
    fn request_spawn_round_trip() {
        let parent_id = AgentId::new();
        let req = Request::Spawn {
            parent_id: parent_id.clone(),
            task: "write tests".to_string(),
            name: Some("write-tests".to_string()),
            adapter: Some("claude".to_string()),
            cwd: std::path::PathBuf::from("/repo"),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"cmd\":\"spawn\""), "should serialize as 'spawn'");
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::Spawn { task, parent_id: p, name: Some(n), .. }
            if task == "write tests" && p == parent_id && n == "write-tests"));
    }

    #[test]
    fn request_spawn_name_defaults_to_none_when_absent() {
        // Older callers (or a hand-written request) may omit `name` entirely —
        // it must deserialize as `None`, not fail to parse.
        let parent_id = AgentId::new();
        let raw = format!(
            r#"{{"cmd":"spawn","parent_id":"{}","task":"write tests","adapter":null,"cwd":"/repo"}}"#,
            parent_id.0
        );
        let req: Request = serde_json::from_str(&raw).unwrap();
        assert!(matches!(req, Request::Spawn { name: None, .. }));
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

    // ── GridSnapshot wire size (real-world perf bug, 2026-07) ─────────────────

    /// A real user reported typing lag and general daemon sluggishness; this
    /// traced back to a full `GridSnapshot`'s JSON size for a realistic
    /// terminal — roughly 1MB, ~60ms to serialize even in a debug build —
    /// generated inline on the daemon's single-threaded ("current_thread")
    /// tokio runtime, stalling every other connection for that whole
    /// duration (fixed in `ipc::server` via `spawn_blocking`). This is a
    /// floor guard, not a target: it fails if the size drops far below what
    /// today's per-cell JSON shape produces, which would mean this test
    /// drifted out of sync with the format rather than the format actually
    /// shrinking — a deliberate size *reduction* (a real fix, not yet built)
    /// should come with an update to this test, not a silent pass.
    #[test]
    fn grid_snapshot_json_size_for_a_realistic_terminal_matches_the_known_cost() {
        let cols = 200usize;
        let lines = 50usize;
        let cells: Vec<Option<CellDto>> = (0..cols * lines)
            .map(|i| {
                Some(CellDto {
                    ch: char::from_u32(97 + (i % 26) as u32).unwrap(),
                    fg: ColorDto::White,
                    bg: ColorDto::Reset,
                    bold: false,
                    italic: false,
                    underline: false,
                    inverse: false,
                })
            })
            .collect();
        let snapshot = GridSnapshot {
            cols: cols as u16,
            lines: lines as u16,
            cells,
            cursor: Some((10, 20)),
            app_cursor_mode: false,
            bracketed_paste_mode: false,
            mouse_reporting_mode: false,
            sgr_mouse_mode: false,
            utf8_mouse_mode: false,
            display_offset: 0,
        };
        let json_len = serde_json::to_string(&snapshot).unwrap().len();
        assert!(
            json_len > 500_000,
            "expected today's per-cell JSON shape to cost roughly ~1MB for a 200x50 grid, got {json_len} bytes -- \
             if this dropped a lot, a real size-reduction landed and this test/comment should be updated to match, \
             not just loosened"
        );
    }
}
