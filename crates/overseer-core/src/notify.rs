//! Attention surfacing (ATTENTION.md): terminal bell + desktop notification
//! on a `→blocked` (and, if configured, `→idle`) transition. Runs entirely in
//! the TUI, on the already-materialized `AgentTree` — identical for `--mock`
//! and a daemon-attached session, since neither backend needs to change for
//! this to work: it's a diff against the previous frame's statuses, not a
//! hook into either backend's event plumbing.

use std::collections::HashMap;

use crate::agent::{AgentId, AgentStatus, FlatNode};
use crate::config::{NotifyConfig, NotifyMode};

/// Pure — every agent whose status this frame differs from what `previous`
/// last recorded for it, `(id, name, new_status)`. A brand-new agent (not in
/// `previous` at all) counts as a transition too: missing the first
/// `blocked` push for a just-spawned agent would be a worse failure mode
/// than an occasional notification on registration.
pub fn status_transitions(
    previous: &HashMap<AgentId, AgentStatus>,
    current: &[FlatNode],
) -> Vec<(AgentId, String, AgentStatus)> {
    current
        .iter()
        .filter(|n| previous.get(&n.id) != Some(&n.status))
        .map(|n| (n.id.clone(), n.name.clone(), n.status.clone()))
        .collect()
}

/// Rebuilds the "last seen" map from the current frame — call after acting on
/// `status_transitions`'s result. Wholesale replacement (not an incremental
/// patch) so a removed agent's stale entry drops out on its own.
pub fn snapshot_statuses(current: &[FlatNode]) -> HashMap<AgentId, AgentStatus> {
    current.iter().map(|n| (n.id.clone(), n.status.clone())).collect()
}

/// One thing to actually do for a transition — the pure policy decision
/// `handle_transitions` executes. Split out so the config-gating logic
/// (bell on/off, mode off/blocked/blocked+idle) is testable without touching
/// real I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Bell,
    Desktop { agent_name: String, message: String },
}

/// Pure — what `config` says to do about each transition. Bell fires on
/// `→blocked` only (if enabled); desktop notification fires on `→blocked`
/// (if `mode` allows it) or `→idle` (only in `BlockedIdle` mode).
fn actions_for(config: &NotifyConfig, transitions: &[(AgentId, String, AgentStatus)]) -> Vec<Action> {
    let mut actions = Vec::new();
    for (_, name, status) in transitions {
        match status {
            AgentStatus::Blocked => {
                if config.bell {
                    actions.push(Action::Bell);
                }
                if matches!(config.mode, NotifyMode::Blocked | NotifyMode::BlockedIdle) {
                    actions.push(Action::Desktop {
                        agent_name: name.clone(),
                        message: "needs approval".to_string(),
                    });
                }
            }
            AgentStatus::Idle if config.mode == NotifyMode::BlockedIdle => {
                actions.push(Action::Desktop {
                    agent_name: name.clone(),
                    message: "finished responding".to_string(),
                });
            }
            _ => {}
        }
    }
    actions
}

/// Applies `config`'s policy to this frame's transitions. The I/O paths
/// (`ring_bell`/`notify_desktop`) are thin, unexercised-by-design shells
/// (house style) — the interesting logic is `actions_for`, pure and tested.
pub fn handle_transitions(config: &NotifyConfig, transitions: &[(AgentId, String, AgentStatus)]) {
    for action in actions_for(config, transitions) {
        match action {
            Action::Bell => ring_bell(),
            Action::Desktop { agent_name, message } => notify_desktop(&agent_name, &message),
        }
    }
}

/// Writes BEL (`\x07`) to this process's own stdout — the zero-dependency
/// notification that works everywhere, including over ssh. What it turns
/// into (a badge, a sound, a dock bounce) is entirely the user's terminal's
/// call.
fn ring_bell() {
    use std::io::Write;
    let _ = std::io::stdout().write_all(b"\x07");
    let _ = std::io::stdout().flush();
}

/// Fire-and-forget desktop notification, never blocking the UI thread and
/// never failing the caller if the OS command is missing. `agent_name` is
/// passed as its own argv element on both platforms — never through a shell,
/// since it's a user/root-agent-authored string (an agent's `--name`).
fn notify_desktop(agent_name: &str, message: &str) {
    let body = format!("{agent_name} {message}");
    #[cfg(target_os = "macos")]
    {
        // Absolute path (SECURITY-AUDIT.md F6): a stable, standard location
        // on macOS, so this can't be shadowed by an earlier `osascript` on
        // an attacker-influenced `$PATH`.
        let script = format!(r#"display notification "{}" with title "Overseer""#, escape_applescript(&body));
        let _ = std::process::Command::new("/usr/bin/osascript").arg("-e").arg(script).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        // `--` (SECURITY-AUDIT.md F7) stops `body` from ever being parsed as
        // a flag if a future refactor drops the trailing message text that
        // today always follows `agent_name`, making a `--`/`-`-prefixed
        // agent name unable to start a bare positional.
        let _ = std::process::Command::new("/usr/bin/notify-send")
            .arg("Overseer")
            .arg("--")
            .arg(&body)
            .spawn();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = body; // no known notifier on this platform — silently a no-op
    }
}

/// Escapes the characters AppleScript string literals treat specially.
/// `notify_desktop` still passes the whole script as one `-e` argv element
/// (never through `sh -c`), but the *AppleScript* string itself needs its own
/// quotes/backslashes escaped or an agent name containing a `"` would break
/// out of the literal.
#[cfg(target_os = "macos")]
fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(id: AgentId, name: &str, status: AgentStatus) -> FlatNode {
        FlatNode {
            id,
            name: name.to_string(),
            status,
            role: crate::agent::AgentRole::Root,
            repo: "repo".to_string(),
            branch: "main".to_string(),
            context_pct: None,
            model_name: None,
            attention: None,
            has_children: false,
            prefix: String::new(),
            status_since: std::time::Instant::now(),
            adapter: "claude".to_string(),
        }
    }

    #[test]
    fn no_previous_entry_counts_as_a_transition() {
        let id = AgentId::new();
        let previous = HashMap::new();
        let current = vec![flat(id.clone(), "agent", AgentStatus::Blocked)];
        let transitions = status_transitions(&previous, &current);
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].0, id);
        assert_eq!(transitions[0].2, AgentStatus::Blocked);
    }

    #[test]
    fn same_status_is_not_a_transition() {
        let id = AgentId::new();
        let mut previous = HashMap::new();
        previous.insert(id.clone(), AgentStatus::Blocked);
        let current = vec![flat(id, "agent", AgentStatus::Blocked)];
        assert!(status_transitions(&previous, &current).is_empty());
    }

    #[test]
    fn running_to_blocked_fires_once() {
        let id = AgentId::new();
        let mut previous = HashMap::new();
        previous.insert(id.clone(), AgentStatus::Running);
        let current = vec![flat(id.clone(), "agent", AgentStatus::Blocked)];
        let transitions = status_transitions(&previous, &current);
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].2, AgentStatus::Blocked);
    }

    #[test]
    fn unrelated_agents_do_not_interfere() {
        let blocked_id = AgentId::new();
        let running_id = AgentId::new();
        let mut previous = HashMap::new();
        previous.insert(blocked_id.clone(), AgentStatus::Running);
        previous.insert(running_id.clone(), AgentStatus::Running);
        let current = vec![
            flat(blocked_id.clone(), "a", AgentStatus::Blocked),
            flat(running_id.clone(), "b", AgentStatus::Running),
        ];
        let transitions = status_transitions(&previous, &current);
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].0, blocked_id);
    }

    #[test]
    fn snapshot_statuses_drops_removed_agents() {
        let id = AgentId::new();
        let current = vec![flat(id.clone(), "agent", AgentStatus::Idle)];
        let snapshot = snapshot_statuses(&current);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.get(&id), Some(&AgentStatus::Idle));

        let empty_snapshot = snapshot_statuses(&[]);
        assert!(empty_snapshot.is_empty());
    }

    // ── actions_for (policy) ──────────────────────────────────────────────────

    fn blocked_transition() -> Vec<(AgentId, String, AgentStatus)> {
        vec![(AgentId::new(), "agent".to_string(), AgentStatus::Blocked)]
    }

    fn idle_transition() -> Vec<(AgentId, String, AgentStatus)> {
        vec![(AgentId::new(), "agent".to_string(), AgentStatus::Idle)]
    }

    #[test]
    fn default_config_rings_the_bell_on_blocked_but_no_desktop_notification() {
        let config = NotifyConfig::default();
        let actions = actions_for(&config, &blocked_transition());
        assert_eq!(actions, vec![Action::Bell]);
    }

    #[test]
    fn bell_false_suppresses_the_bell_on_a_blocked_transition() {
        let config = NotifyConfig { bell: false, mode: NotifyMode::Off };
        let actions = actions_for(&config, &blocked_transition());
        assert!(actions.is_empty(), "bell=false must suppress the bell action entirely");
    }

    #[test]
    fn mode_blocked_adds_a_desktop_notification_alongside_the_bell() {
        let config = NotifyConfig { bell: true, mode: NotifyMode::Blocked };
        let actions = actions_for(&config, &blocked_transition());
        assert_eq!(actions.len(), 2);
        assert!(actions.contains(&Action::Bell));
        assert!(actions.iter().any(|a| matches!(a, Action::Desktop { message, .. } if message == "needs approval")));
    }

    #[test]
    fn mode_off_never_fires_a_desktop_notification_on_idle() {
        let config = NotifyConfig { bell: true, mode: NotifyMode::Off };
        assert!(actions_for(&config, &idle_transition()).is_empty());
    }

    #[test]
    fn mode_blocked_alone_does_not_notify_on_idle() {
        let config = NotifyConfig { bell: true, mode: NotifyMode::Blocked };
        assert!(actions_for(&config, &idle_transition()).is_empty());
    }

    #[test]
    fn mode_blocked_plus_idle_notifies_on_idle_too() {
        let config = NotifyConfig { bell: true, mode: NotifyMode::BlockedIdle };
        let actions = actions_for(&config, &idle_transition());
        assert_eq!(actions, vec![Action::Desktop {
            agent_name: "agent".to_string(),
            message: "finished responding".to_string(),
        }]);
    }

    #[test]
    fn running_and_other_statuses_never_produce_actions() {
        let config = NotifyConfig { bell: true, mode: NotifyMode::BlockedIdle };
        for status in [AgentStatus::Running, AgentStatus::Spawning, AgentStatus::Done, AgentStatus::Error] {
            let transitions = vec![(AgentId::new(), "agent".to_string(), status)];
            assert!(actions_for(&config, &transitions).is_empty());
        }
    }
}
