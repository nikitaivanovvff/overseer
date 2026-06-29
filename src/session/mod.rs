use anyhow::{Context, Result};
use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub name: String,
    pub windows: usize,
}

/// Parse a single line from `tmux list-sessions -F "#{session_name}|#{session_windows}"`.
pub fn parse_session_line(line: &str) -> Option<SessionInfo> {
    let (name, rest) = line.rsplit_once('|')?;
    Some(SessionInfo {
        name: name.trim().to_string(),
        windows: rest.trim().parse().unwrap_or(0),
    })
}

/// Parse the full stdout output of the tmux list-sessions command.
pub fn parse_sessions(output: &str) -> Vec<SessionInfo> {
    output.lines().filter_map(parse_session_line).collect()
}

pub struct TmuxClient;

impl TmuxClient {
    pub fn new() -> Self {
        Self
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let output = Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name}|#{session_windows}"])
            .output()
            .context("failed to run tmux")?;

        if !output.status.success() {
            // tmux exits non-zero when there are no sessions; treat as empty.
            return Ok(Vec::new());
        }

        Ok(parse_sessions(&String::from_utf8_lossy(&output.stdout)))
    }

    pub fn new_session(&self, name: &str, start_dir: &str) -> Result<()> {
        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", name, "-c", start_dir])
            .status()
            .context("failed to run tmux new-session")?;

        anyhow::ensure!(status.success(), "tmux new-session failed for '{name}'");
        Ok(())
    }

    pub fn kill_session(&self, name: &str) -> Result<()> {
        let status = Command::new("tmux")
            .args(["kill-session", "-t", name])
            .status()
            .context("failed to run tmux kill-session")?;

        anyhow::ensure!(status.success(), "tmux kill-session failed for '{name}'");
        Ok(())
    }

    pub fn session_exists(&self, name: &str) -> bool {
        Command::new("tmux")
            .args(["has-session", "-t", name])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

impl Default for TmuxClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_line() {
        let info = parse_session_line("my-session|3").unwrap();
        assert_eq!(info.name, "my-session");
        assert_eq!(info.windows, 3);
    }

    #[test]
    fn test_parse_line_with_whitespace() {
        let info = parse_session_line("  session-name | 5  ").unwrap();
        assert_eq!(info.name, "session-name");
        assert_eq!(info.windows, 5);
    }

    #[test]
    fn test_parse_invalid_windows_defaults_to_zero() {
        let info = parse_session_line("session|not-a-number").unwrap();
        assert_eq!(info.windows, 0);
    }

    #[test]
    fn test_parse_missing_separator_returns_none() {
        assert!(parse_session_line("no-separator").is_none());
    }

    #[test]
    fn test_parse_empty_line_returns_none() {
        assert!(parse_session_line("").is_none());
    }

    #[test]
    fn test_parse_windows_zero() {
        let info = parse_session_line("empty-session|0").unwrap();
        assert_eq!(info.windows, 0);
    }

    #[test]
    fn test_parse_large_window_count() {
        let info = parse_session_line("busy|42").unwrap();
        assert_eq!(info.windows, 42);
    }

    #[test]
    fn test_parse_sessions_multiple_lines() {
        let output = "alpha|2\nbeta|1\ngamma|4\n";
        let sessions = parse_sessions(output);
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].name, "alpha");
        assert_eq!(sessions[0].windows, 2);
        assert_eq!(sessions[1].name, "beta");
        assert_eq!(sessions[2].name, "gamma");
        assert_eq!(sessions[2].windows, 4);
    }

    #[test]
    fn test_parse_sessions_empty_output() {
        assert!(parse_sessions("").is_empty());
    }

    #[test]
    fn test_parse_sessions_skips_blank_lines() {
        let output = "alpha|2\n\nbeta|1\n";
        let sessions = parse_sessions(output);
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn test_parse_sessions_skips_malformed_lines() {
        let output = "good|1\nbadline\nalso-good|3\n";
        let sessions = parse_sessions(output);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "good");
        assert_eq!(sessions[1].name, "also-good");
    }

    #[test]
    fn test_session_info_equality() {
        let a = SessionInfo { name: "x".into(), windows: 1 };
        let b = SessionInfo { name: "x".into(), windows: 1 };
        assert_eq!(a, b);
    }

    #[test]
    fn test_session_info_inequality() {
        let a = SessionInfo { name: "x".into(), windows: 1 };
        let b = SessionInfo { name: "x".into(), windows: 2 };
        assert_ne!(a, b);
    }
}
