use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
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

pub struct TmuxClient {
    dry_run: bool,
}

impl TmuxClient {
    pub fn new() -> Self {
        Self { dry_run: false }
    }

    /// Returns a no-op client that succeeds without invoking tmux — for tests.
    #[cfg(test)]
    pub fn dry_run() -> Self {
        Self { dry_run: true }
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        if self.dry_run {
            return Ok(Vec::new());
        }
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
        if self.dry_run {
            return Ok(());
        }
        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", name, "-c", start_dir])
            .status()
            .context("failed to run tmux new-session")?;

        anyhow::ensure!(status.success(), "tmux new-session failed for '{name}'");
        Ok(())
    }

    pub fn kill_session(&self, name: &str) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let status = Command::new("tmux")
            .args(["kill-session", "-t", name])
            .status()
            .context("failed to run tmux kill-session")?;

        anyhow::ensure!(status.success(), "tmux kill-session failed for '{name}'");
        Ok(())
    }

    pub fn session_exists(&self, name: &str) -> bool {
        if self.dry_run {
            return false;
        }
        Command::new("tmux")
            .args(["has-session", "-t", name])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Launches `cmd` in a new detached tmux session with the given env vars injected.
    ///
    /// Builds: `tmux new-session -d -s <name> -c <cwd> -e K=V ... -- <program> <args>`
    /// The `-e` flag requires tmux >= 3.0.
    pub fn launch(
        &self,
        name: &str,
        cwd: &Path,
        cmd: &Command,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        self.check_min_version(3, 0)?;

        let program = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> =
            cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();

        let mut tmux_cmd = Command::new("tmux");
        tmux_cmd.args(["new-session", "-d", "-s", name, "-c"]);
        tmux_cmd.arg(cwd);
        for (k, v) in env {
            tmux_cmd.arg("-e");
            tmux_cmd.arg(format!("{k}={v}"));
        }
        tmux_cmd.arg("--");
        tmux_cmd.arg(&program);
        for arg in &args {
            tmux_cmd.arg(arg);
        }

        let status = tmux_cmd.status().context("failed to run tmux new-session")?;
        anyhow::ensure!(status.success(), "tmux new-session failed for session '{name}'");
        Ok(())
    }

    fn check_min_version(&self, major: u32, minor: u32) -> Result<()> {
        let output = Command::new("tmux")
            .arg("-V")
            .output()
            .context("failed to query tmux version")?;
        let version_str = String::from_utf8_lossy(&output.stdout);
        let ver_part = version_str.trim().strip_prefix("tmux ").unwrap_or("");
        let (maj, min) = parse_tmux_version(ver_part);
        anyhow::ensure!(
            (maj, min) >= (major, minor),
            "tmux >= {major}.{minor} required (found {})",
            version_str.trim()
        );
        Ok(())
    }
}

fn parse_tmux_version(ver: &str) -> (u32, u32) {
    // Strip trailing alpha chars: "3.3a" → "3.3"
    let clean: String = ver.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let mut parts = clean.splitn(2, '.');
    let major = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (major, minor)
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

    #[test]
    fn parse_tmux_version_stable() {
        assert_eq!(parse_tmux_version("3.3a"), (3, 3));
        assert_eq!(parse_tmux_version("3.0"), (3, 0));
        assert_eq!(parse_tmux_version("2.9"), (2, 9));
        assert_eq!(parse_tmux_version("3.3"), (3, 3));
    }

    #[test]
    fn parse_tmux_version_unknown() {
        assert_eq!(parse_tmux_version(""), (0, 0));
        assert_eq!(parse_tmux_version("garbage"), (0, 0));
    }

    #[test]
    fn dry_run_launch_is_noop() {
        let t = TmuxClient::dry_run();
        let cmd = Command::new("claude");
        let env = HashMap::new();
        t.launch("test-session", Path::new("/tmp"), &cmd, &env).unwrap();
    }
}
