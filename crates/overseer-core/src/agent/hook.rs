//! Pure parsing for Claude Code hook payloads, piped to `overseer status --from-hook`
//! on stdin. Kept separate from the CLI's I/O (stdin read, socket send) so the
//! actual parsing/classification logic is unit-testable over plain `&str`.

use serde::Deserialize;

use super::AgentStatus;

#[derive(Debug, Default, Deserialize)]
pub struct HookPayload {
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Parses a hook's stdin JSON. `None` on any malformed input — callers must never
/// fail the hook over this, just push the status without the extra context.
pub fn parse_hook_payload(raw: &str) -> Option<HookPayload> {
    serde_json::from_str(raw).ok()
}

/// Returns the model from the newest real assistant turn in a Claude Code
/// transcript. Synthetic local messages are not model responses and must not
/// replace a previously known model.
pub fn latest_model_from_transcript(raw: &str) -> Option<String> {
    raw.lines().rev().find_map(|line| {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        if value.get("type")?.as_str()? != "assistant" {
            return None;
        }
        let model = value.get("message")?.get("model")?.as_str()?;
        (!model.is_empty() && model != "<synthetic>").then(|| model.to_string())
    })
}

/// `Notification` fires for both permission requests and the ~60s idle nag. Only
/// the idle nag should be downgraded from `blocked` to `idle` — everything else
/// (permission prompts) stays `blocked`. Matched by substring against the known
/// Claude Code idle-nag wording rather than an exact match, so minor wording
/// changes upstream don't silently break the classification.
pub fn is_idle_nag(message: &str) -> bool {
    message.to_lowercase().contains("waiting for your input")
}

/// Pure. Only a `blocked` push needs classification — every other status
/// already means what it says. `Notification` fires for both a real permission
/// request and the ~60s idle nag; a missing/unparsed payload leaves `blocked`
/// as-is (the safer default — a permission prompt actually pending).
pub fn classify_hook_status(status: AgentStatus, payload: Option<&HookPayload>) -> AgentStatus {
    if status != AgentStatus::Blocked {
        return status;
    }
    match payload.and_then(|p| p.message.as_deref()) {
        Some(msg) if is_idle_nag(msg) => AgentStatus::Idle,
        _ => AgentStatus::Blocked,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hook_payload_extracts_known_fields() {
        let raw = r#"{"transcript_path":"/tmp/t.jsonl","message":"Claude needs your permission to use Bash","hook_event_name":"Notification"}"#;
        let payload = parse_hook_payload(raw).unwrap();
        assert_eq!(payload.transcript_path.as_deref(), Some("/tmp/t.jsonl"));
        assert_eq!(payload.message.as_deref(), Some("Claude needs your permission to use Bash"));
    }

    #[test]
    fn parse_hook_payload_missing_fields_are_none() {
        let payload = parse_hook_payload(r#"{"hook_event_name":"Stop"}"#).unwrap();
        assert!(payload.transcript_path.is_none());
        assert!(payload.message.is_none());
    }

    #[test]
    fn parse_hook_payload_garbage_returns_none() {
        assert!(parse_hook_payload("not json at all").is_none());
        assert!(parse_hook_payload("").is_none());
    }

    #[test]
    fn latest_model_uses_newest_real_assistant_turn() {
        let transcript = concat!(
            r#"{"type":"assistant","message":{"model":"claude-opus-4-1"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"next"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"model":"claude-sonnet-5"}}"#,
        );
        assert_eq!(latest_model_from_transcript(transcript).as_deref(), Some("claude-sonnet-5"));
    }

    #[test]
    fn latest_model_ignores_synthetic_and_malformed_entries() {
        let transcript = concat!(
            r#"{"type":"assistant","message":{"model":"claude-sonnet-5"}}"#,
            "\nnot json\n",
            r#"{"type":"assistant","message":{"model":"<synthetic>"}}"#,
        );
        assert_eq!(latest_model_from_transcript(transcript).as_deref(), Some("claude-sonnet-5"));
    }

    #[test]
    fn is_idle_nag_matches_waiting_for_input() {
        assert!(is_idle_nag("Claude is waiting for your input"));
        assert!(is_idle_nag("claude is WAITING FOR YOUR INPUT"));
    }

    #[test]
    fn is_idle_nag_false_for_permission_request() {
        assert!(!is_idle_nag("Claude needs your permission to use Bash"));
        assert!(!is_idle_nag("Claude needs your permission to edit files"));
    }

    // ── classify_hook_status ──────────────────────────────────────────────────

    #[test]
    fn classify_hook_status_leaves_non_blocked_untouched() {
        assert_eq!(classify_hook_status(AgentStatus::Running, None), AgentStatus::Running);
        assert_eq!(classify_hook_status(AgentStatus::Idle, None), AgentStatus::Idle);
    }

    #[test]
    fn classify_hook_status_no_payload_stays_blocked() {
        assert_eq!(classify_hook_status(AgentStatus::Blocked, None), AgentStatus::Blocked);
    }

    #[test]
    fn classify_hook_status_permission_request_stays_blocked() {
        let payload = HookPayload {
            transcript_path: None,
            message: Some("Claude needs your permission to use Bash".to_string()),
        };
        assert_eq!(classify_hook_status(AgentStatus::Blocked, Some(&payload)), AgentStatus::Blocked);
    }

    #[test]
    fn classify_hook_status_idle_nag_downgrades_to_idle() {
        let payload = HookPayload {
            transcript_path: None,
            message: Some("Claude is waiting for your input".to_string()),
        };
        assert_eq!(classify_hook_status(AgentStatus::Blocked, Some(&payload)), AgentStatus::Idle);
    }

}
