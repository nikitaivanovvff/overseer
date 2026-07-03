use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// All Overseer tmux sessions (TUI + agents) live on this private server, isolated
/// from the user's own default tmux server. See PHASE5.md §2.
const SERVER: &str = "overseer";

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
    fail_launch: bool,
    /// `Some(names)` in dry-run mode reports exactly those session names as existing
    /// (everything else dead); `None` reports everything dead. Test-only knob.
    live_sessions: Option<std::collections::HashSet<String>>,
}

impl TmuxClient {
    pub fn new() -> Self {
        Self { dry_run: false, fail_launch: false, live_sessions: None }
    }

    /// Returns a no-op client that succeeds without invoking tmux — for tests, and
    /// for `--mock` so seeded demo data never launches a real tmux session.
    pub fn dry_run() -> Self {
        Self { dry_run: true, fail_launch: false, live_sessions: None }
    }

    #[cfg(test)]
    pub(crate) fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// A dry-run client whose `launch()` always fails — for testing rollback behavior
    /// on launch failure without depending on a real, misconfigured tmux invocation.
    #[cfg(test)]
    pub fn dry_run_failing_launch() -> Self {
        Self { dry_run: true, fail_launch: true, live_sessions: None }
    }

    /// A dry-run client that reports only `live` as existing sessions — for testing
    /// code that must distinguish which of several agents' sessions are still alive.
    #[cfg(test)]
    pub fn dry_run_with_live_sessions(live: std::collections::HashSet<String>) -> Self {
        Self { dry_run: true, fail_launch: false, live_sessions: Some(live) }
    }

    /// Base command for every real tmux invocation, pre-loaded with `-L overseer` so
    /// all Overseer sessions live on the private server (AGENTS.md: `TmuxClient` is
    /// the only tmux boundary — every verb below builds from this).
    fn tmux(&self) -> Command {
        let mut cmd = Command::new("tmux");
        cmd.args(["-L", SERVER]);
        cmd
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        if self.dry_run {
            return Ok(Vec::new());
        }
        let output = self
            .tmux()
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
        let status = self
            .tmux()
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
        let status = self
            .tmux()
            .args(["kill-session", "-t", name])
            .status()
            .context("failed to run tmux kill-session")?;

        anyhow::ensure!(status.success(), "tmux kill-session failed for '{name}'");
        Ok(())
    }

    pub fn session_exists(&self, name: &str) -> bool {
        if self.dry_run {
            return self.live_sessions.as_ref().is_some_and(|live| live.contains(name));
        }
        self.tmux()
            .args(["has-session", "-t", name])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Creates the permanent live-agent pane once at TUI startup: splits `target`'s
    /// window, running `cmd` in the new pane at `size_pct`% width. `-d`: doesn't
    /// steal focus from the pane that's splitting. Returns the new pane's id
    /// (e.g. `"%3"`), for `pane_tty`/`select_pane`/`set_remain_on_exit`.
    pub fn split_pane(&self, target: &str, size_pct: u8, cmd: &Command) -> Result<String> {
        if self.dry_run {
            return Ok("%0".to_string());
        }
        let program = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> =
            cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();

        let mut tmux_cmd = self.tmux();
        tmux_cmd.args([
            "split-window",
            "-t",
            target,
            "-h",
            "-p",
            &size_pct.to_string(),
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "--",
        ]);
        tmux_cmd.arg(&program);
        for arg in &args {
            tmux_cmd.arg(arg);
        }

        let output = tmux_cmd.output().context("failed to run tmux split-window")?;
        anyhow::ensure!(output.status.success(), "tmux split-window failed for target '{target}'");
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// The tty a pane's own attached client registers as — captured once right
    /// after `split_pane`, since the nested client running in that pane will
    /// report exactly this tty. The `-c` target for `switch_client_on`.
    pub fn pane_tty(&self, pane_id: &str) -> Result<String> {
        if self.dry_run {
            return Ok("/dev/dry-run-tty".to_string());
        }
        let output = self
            .tmux()
            .args(["display-message", "-p", "-t", pane_id, "#{pane_tty}"])
            .output()
            .context("failed to run tmux display-message")?;
        anyhow::ensure!(output.status.success(), "tmux display-message failed for pane '{pane_id}'");
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Retargets the *specific* client at `client_tty` to `session` — unlike a
    /// plain `switch-client -t`, this never touches the user's own outer client,
    /// only the nested client running inside the live pane.
    pub fn switch_client_on(&self, client_tty: &str, session: &str) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let status = self
            .tmux()
            .args(["switch-client", "-c", client_tty, "-t", session])
            .status()
            .context("failed to run tmux switch-client")?;

        anyhow::ensure!(
            status.success(),
            "tmux switch-client failed for '{session}' on client '{client_tty}'"
        );
        Ok(())
    }

    /// Moves tmux's keyboard focus into `pane_id` — this is "jump in": the pane
    /// was already showing the agent's real, live session, so entering it is
    /// just a focus change, not a client switch.
    pub fn select_pane(&self, pane_id: &str) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let status = self
            .tmux()
            .args(["select-pane", "-t", pane_id])
            .status()
            .context("failed to run tmux select-pane")?;

        anyhow::ensure!(status.success(), "tmux select-pane failed for '{pane_id}'");
        Ok(())
    }

    /// Set once, right after `split_pane`: keeps the live pane alive when its
    /// nested client's session is killed (attached-client death otherwise
    /// collapses the split entirely).
    pub fn set_remain_on_exit(&self, pane_id: &str, on: bool) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let value = if on { "on" } else { "off" };
        let status = self
            .tmux()
            .args(["set-option", "-p", "-t", pane_id, "remain-on-exit", value])
            .status()
            .context("failed to run tmux set-option")?;

        anyhow::ensure!(status.success(), "tmux set-option remain-on-exit failed for '{pane_id}'");
        Ok(())
    }

    /// Turns off tmux's own status line, server-wide (`-g`, so it applies to
    /// every current and future session on the private server — the home
    /// session, the placeholder, and every agent). Overseer's own ratatui
    /// status bar already shows equivalent info; tmux's default chrome on top
    /// of that (and again, separately, for the nested client in the live
    /// pane) is pure noise the user never asked for.
    pub fn disable_status_bar(&self) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let status = self
            .tmux()
            .args(["set-option", "-g", "status", "off"])
            .status()
            .context("failed to run tmux set-option status off")?;

        anyhow::ensure!(status.success(), "tmux set-option status off failed");
        Ok(())
    }

    /// Recreates the process running in `pane_id` (which must have
    /// `remain-on-exit` set, so it stays around dead rather than closing when
    /// its old client exits). Self-heal for the live pane: if its nested
    /// client's session was killed by anything other than the TUI's own retarget
    /// (an IPC `overseer drop`, the dead-session watcher, or the session dying
    /// on its own), `switch_client_on` has nothing left to redirect — respawning
    /// starts a *fresh* nested client already pointed at wherever the tree wants
    /// it, so the pane recovers within one tick regardless of what killed it.
    pub fn respawn_pane(&self, pane_id: &str, cmd: &Command) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let program = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> =
            cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();

        let mut tmux_cmd = self.tmux();
        tmux_cmd.args(["respawn-pane", "-k", "-t", pane_id, "--"]);
        tmux_cmd.arg(&program);
        for arg in &args {
            tmux_cmd.arg(arg);
        }

        let status = tmux_cmd.status().context("failed to run tmux respawn-pane")?;
        anyhow::ensure!(status.success(), "tmux respawn-pane failed for '{pane_id}'");
        Ok(())
    }

    /// Attaches the current terminal to `session`, blocking until the client detaches
    /// (session killed, `tmux detach`, etc.). Used by the bootstrap in `main.rs` to
    /// host the Overseer TUI inside its own tmux session.
    pub fn attach_session(&self, session: &str) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let status = self
            .tmux()
            .args(["attach-session", "-t", session])
            .status()
            .context("failed to run tmux attach-session")?;

        anyhow::ensure!(status.success(), "tmux attach-session failed for '{session}'");
        Ok(())
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
            anyhow::ensure!(!self.fail_launch, "simulated launch failure for '{name}'");
            return Ok(());
        }
        self.check_min_version(3, 0)?;

        let program = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> =
            cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();

        let mut tmux_cmd = self.tmux();
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
        let output = self
            .tmux()
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

/// The command run inside the permanent live-agent pane: a second tmux client of
/// our own private server, attached to whichever agent session is currently
/// selected. Retargeted afterward via `switch_client_on` — never restarted.
///
/// This pane already lives inside a `-L overseer` session, so its own shell
/// environment inherits `$TMUX` pointing at that same server; tmux refuses to
/// attach in that case ("sessions should be nested with care") unless `$TMUX` is
/// cleared first. Confirmed live in the Phase 5c spike (session-internal nested
/// attach is a different scenario from the outer bootstrap's attach in `main.rs`,
/// which runs from a process with no `$TMUX` set at all). Args are passed
/// positionally (`$1`/`$2`) rather than interpolated into the script string, so
/// this stays safe even though session/server names are currently always simple.
pub fn nested_attach_command(session: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "unset TMUX; exec tmux -L \"$1\" attach -t \"$2\"", "sh", SERVER, session]);
    cmd
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

    #[test]
    fn dry_run_attach_session_is_noop() {
        let t = TmuxClient::dry_run();
        t.attach_session("overseer").unwrap();
    }

    #[test]
    fn dry_run_split_pane_returns_canned_pane_id() {
        let t = TmuxClient::dry_run();
        let cmd = Command::new("tmux");
        let pane_id = t.split_pane("overseer", 75, &cmd).unwrap();
        assert!(!pane_id.is_empty());
    }

    #[test]
    fn dry_run_pane_tty_returns_canned_tty() {
        let t = TmuxClient::dry_run();
        let tty = t.pane_tty("%0").unwrap();
        assert!(!tty.is_empty());
    }

    #[test]
    fn dry_run_switch_client_on_is_noop() {
        let t = TmuxClient::dry_run();
        t.switch_client_on("/dev/ttys000", "test-session").unwrap();
    }

    #[test]
    fn dry_run_select_pane_is_noop() {
        let t = TmuxClient::dry_run();
        t.select_pane("%0").unwrap();
    }

    #[test]
    fn dry_run_set_remain_on_exit_is_noop() {
        let t = TmuxClient::dry_run();
        t.set_remain_on_exit("%0", true).unwrap();
    }

    #[test]
    fn dry_run_disable_status_bar_is_noop() {
        let t = TmuxClient::dry_run();
        t.disable_status_bar().unwrap();
    }

    #[test]
    fn dry_run_respawn_pane_is_noop() {
        let t = TmuxClient::dry_run();
        let cmd = Command::new("tmux");
        t.respawn_pane("%0", &cmd).unwrap();
    }

    #[test]
    fn nested_attach_command_unsets_tmux_before_attaching() {
        let cmd = nested_attach_command("overseer-abcd1234");
        assert_eq!(cmd.get_program(), "sh");
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert!(args[1].contains("unset TMUX"), "must clear $TMUX before the nested attach: {args:?}");
        assert!(args.contains(&"overseer-abcd1234".to_string()));
        assert!(args.contains(&SERVER.to_string()));
    }
}
