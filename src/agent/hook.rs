//! Pure parsing for Claude Code hook payloads, piped to `overseer status --from-hook`
//! on stdin. Kept separate from the CLI's I/O (stdin read, socket send) so the
//! actual parsing/classification logic is unit-testable over plain `&str`.

use serde::Deserialize;

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

/// `Notification` fires for both permission requests and the ~60s idle nag. Only
/// the idle nag should be downgraded from `blocked` to `idle` — everything else
/// (permission prompts) stays `blocked`. Matched by substring against the known
/// Claude Code idle-nag wording rather than an exact match, so minor wording
/// changes upstream don't silently break the classification.
pub fn is_idle_nag(message: &str) -> bool {
    message.to_lowercase().contains("waiting for your input")
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
    fn is_idle_nag_matches_waiting_for_input() {
        assert!(is_idle_nag("Claude is waiting for your input"));
        assert!(is_idle_nag("claude is WAITING FOR YOUR INPUT"));
    }

    #[test]
    fn is_idle_nag_false_for_permission_request() {
        assert!(!is_idle_nag("Claude needs your permission to use Bash"));
        assert!(!is_idle_nag("Claude needs your permission to edit files"));
    }
}
