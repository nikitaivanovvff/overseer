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

#[derive(Debug, Default, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

const CONTEXT_WINDOW_TOKENS: f64 = 200_000.0;

/// Extracts the `usage` object from one transcript JSONL line, if present.
/// Claude Code transcripts nest it under `message.usage` for assistant turns;
/// tolerate a top-level `usage` too rather than assume one exact shape.
fn extract_usage(line: &str) -> Option<Usage> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let usage = value.get("message").and_then(|m| m.get("usage")).or_else(|| value.get("usage"))?;
    serde_json::from_value(usage.clone()).ok()
}

/// Pure. Scans a transcript JSONL file's contents from the end for the most
/// recent entry carrying token usage, and converts it to a 0-100 percentage of
/// the model's context window. `None` if no line in the transcript has usage
/// (e.g. an empty/fresh transcript) — callers push the status without a pct
/// rather than treat that as an error.
pub fn context_pct_from_transcript(contents: &str) -> Option<u8> {
    for line in contents.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(usage) = extract_usage(line) {
            let total = (usage.input_tokens
                + usage.cache_read_input_tokens
                + usage.cache_creation_input_tokens) as f64;
            let pct = (total / CONTEXT_WINDOW_TOKENS * 100.0).round().clamp(0.0, 100.0);
            return Some(pct as u8);
        }
    }
    None
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

    // ── context_pct_from_transcript ───────────────────────────────────────────

    fn transcript_line(input: u64, cache_read: u64, cache_creation: u64) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":{input},"cache_read_input_tokens":{cache_read},"cache_creation_input_tokens":{cache_creation}}}}}}}"#
        )
    }

    #[test]
    fn context_pct_computes_percentage_of_200k_window() {
        // 100_000 total tokens / 200_000 window = 50%.
        let transcript = transcript_line(50_000, 30_000, 20_000);
        assert_eq!(context_pct_from_transcript(&transcript), Some(50));
    }

    #[test]
    fn context_pct_uses_the_last_usage_entry_not_the_first() {
        let transcript = format!(
            "{}\n{}\n{}",
            transcript_line(1_000, 0, 0),
            r#"{"type":"user","message":{"content":"no usage here"}}"#,
            transcript_line(180_000, 0, 0), // 90%
        );
        assert_eq!(context_pct_from_transcript(&transcript), Some(90));
    }

    #[test]
    fn context_pct_top_level_usage_is_also_recognized() {
        let line = r#"{"usage":{"input_tokens":100000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#;
        assert_eq!(context_pct_from_transcript(line), Some(50));
    }

    #[test]
    fn context_pct_clamps_at_100() {
        let transcript = transcript_line(300_000, 0, 0);
        assert_eq!(context_pct_from_transcript(&transcript), Some(100));
    }

    #[test]
    fn context_pct_none_when_no_usage_present() {
        let transcript = r#"{"type":"user","message":{"content":"hello"}}"#;
        assert_eq!(context_pct_from_transcript(transcript), None);
    }

    #[test]
    fn context_pct_none_for_empty_transcript() {
        assert_eq!(context_pct_from_transcript(""), None);
    }

    #[test]
    fn context_pct_ignores_blank_lines_and_garbage() {
        let transcript = format!("\n   \nnot json\n{}\n", transcript_line(20_000, 0, 0));
        assert_eq!(context_pct_from_transcript(&transcript), Some(10));
    }
}
