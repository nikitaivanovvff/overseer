//! Minimal `[defaults]`/`[adapters.*]` config loader (AGENTS.md "Config").
//! Keybindings/theme are still Phase 5b — not modeled here.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone)]
pub struct Config {
    pub defaults: Defaults,
    pub adapters: HashMap<String, AdapterConfig>,
    pub notify: NotifyConfig,
}

/// Deserializes the raw TOML shape, then merges the user's `[adapters.*]` on
/// top of the built-in defaults instead of letting it replace the whole map —
/// a file that only overrides `[adapters.aider]` must not lose the built-in
/// `claude` entry it never mentioned. This is a property of `Config` itself
/// (not a post-`load()` patch) so it holds for any caller that deserializes a
/// `Config`, not just the real `~/.config/overseer/config.toml` load path.
impl<'de> Deserialize<'de> for Config {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawConfig {
            #[serde(default)]
            defaults: Defaults,
            #[serde(default)]
            adapters: HashMap<String, AdapterConfig>,
            #[serde(default)]
            notify: NotifyConfig,
        }

        let raw = RawConfig::deserialize(deserializer)?;
        let mut adapters = default_adapters();
        adapters.extend(raw.adapters);
        Ok(Config { defaults: raw.defaults, adapters, notify: raw.notify })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Defaults {
    #[serde(default = "default_adapter_name")]
    pub adapter: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdapterConfig {
    pub command: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// `[notify]` — every attention-surfacing channel is independently
/// switchable off (ATTENTION.md's user requirement). The bell defaults on
/// (it's inert unless the user's terminal makes it loud); desktop
/// notifications default off (an opt-in, louder channel).
#[derive(Debug, Clone, Deserialize)]
pub struct NotifyConfig {
    #[serde(default = "default_bell")]
    pub bell: bool,
    #[serde(default)]
    pub mode: NotifyMode,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self { bell: default_bell(), mode: NotifyMode::default() }
    }
}

fn default_bell() -> bool {
    true
}

/// Desktop-notification scope. `BlockedIdle` also fires on `→idle` — for
/// long tasks where "it finished responding" is itself worth a ping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyMode {
    #[default]
    Off,
    Blocked,
    #[serde(rename = "blocked+idle")]
    BlockedIdle,
}

fn default_adapter_name() -> String {
    "claude".to_string()
}

fn default_adapters() -> HashMap<String, AdapterConfig> {
    let mut adapters = HashMap::new();
    adapters.insert(
        "claude".to_string(),
        AdapterConfig { command: "claude".to_string(), extra_args: vec![] },
    );
    adapters
}

impl Default for Defaults {
    fn default() -> Self {
        Self { adapter: default_adapter_name() }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self { defaults: Defaults::default(), adapters: default_adapters(), notify: NotifyConfig::default() }
    }
}

impl Config {
    /// Loads from `~/.config/overseer/config.toml`. Missing file, unreadable file,
    /// or invalid TOML all fall back to the built-in default — a config problem
    /// must never prevent the TUI from starting.
    pub fn load() -> Self {
        Self::load_from(&default_config_path())
    }

    fn load_from(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => toml::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }
}

/// `~/.config/overseer/config.toml` — an explicit, cross-platform path (AGENTS.md),
/// not `dirs::config_dir()` (which resolves to `~/Library/Application Support` on
/// macOS, not what the docs promise).
fn default_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("overseer")
        .join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_claude_adapter_and_default_adapter_name() {
        let cfg = Config::default();
        assert_eq!(cfg.defaults.adapter, "claude");
        let claude = cfg.adapters.get("claude").expect("default config must have claude");
        assert_eq!(claude.command, "claude");
        assert!(claude.extra_args.is_empty());
    }

    #[test]
    fn load_from_missing_file_returns_default() {
        let cfg = Config::load_from(std::path::Path::new("/nonexistent/overseer/config.toml"));
        assert_eq!(cfg.defaults.adapter, "claude");
    }

    #[test]
    fn parses_sample_toml() {
        let raw = r#"
            [defaults]
            adapter = "aider"

            [adapters.claude]
            command = "claude"
            extra_args = ["--dangerously-skip-permissions"]

            [adapters.aider]
            command = "/usr/local/bin/aider"
            extra_args = []
        "#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.defaults.adapter, "aider");
        let claude = cfg.adapters.get("claude").unwrap();
        assert_eq!(claude.extra_args, vec!["--dangerously-skip-permissions".to_string()]);
        let aider = cfg.adapters.get("aider").unwrap();
        assert_eq!(aider.command, "/usr/local/bin/aider");
    }

    #[test]
    fn partial_adapters_override_keeps_the_built_in_claude_entry() {
        // A file that only adds a second adapter (the exact AGENTS.md example)
        // must not lose the built-in "claude" entry it never mentioned.
        let raw = r#"
            [adapters.aider]
            command = "/usr/local/bin/aider"
            extra_args = []
        "#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.defaults.adapter, "claude"); // untouched default
        let claude = cfg.adapters.get("claude").expect("claude must survive a partial override");
        assert_eq!(claude.command, "claude");
        let aider = cfg.adapters.get("aider").unwrap();
        assert_eq!(aider.command, "/usr/local/bin/aider");
    }

    #[test]
    fn explicit_claude_override_replaces_the_default_entry() {
        let raw = r#"
            [adapters.claude]
            command = "/opt/claude-wrapper"
            extra_args = ["--foo"]
        "#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let claude = cfg.adapters.get("claude").unwrap();
        assert_eq!(claude.command, "/opt/claude-wrapper");
        assert_eq!(claude.extra_args, vec!["--foo".to_string()]);
    }

    // ── [notify] (ATTENTION.md) ───────────────────────────────────────────────

    #[test]
    fn notify_defaults_bell_on_desktop_off() {
        let cfg = Config::default();
        assert!(cfg.notify.bell);
        assert_eq!(cfg.notify.mode, NotifyMode::Off);
    }

    #[test]
    fn notify_defaults_when_section_is_absent_from_the_file() {
        let cfg: Config = toml::from_str("[defaults]\nadapter = \"claude\"\n").unwrap();
        assert!(cfg.notify.bell);
        assert_eq!(cfg.notify.mode, NotifyMode::Off);
    }

    #[test]
    fn notify_bell_false_parses() {
        let cfg: Config = toml::from_str("[notify]\nbell = false\n").unwrap();
        assert!(!cfg.notify.bell);
    }

    #[test]
    fn notify_mode_blocked_parses() {
        let cfg: Config = toml::from_str("[notify]\nmode = \"blocked\"\n").unwrap();
        assert_eq!(cfg.notify.mode, NotifyMode::Blocked);
    }

    #[test]
    fn notify_mode_blocked_plus_idle_parses() {
        let cfg: Config = toml::from_str("[notify]\nmode = \"blocked+idle\"\n").unwrap();
        assert_eq!(cfg.notify.mode, NotifyMode::BlockedIdle);
    }

    #[test]
    fn notify_bell_and_mode_together() {
        let cfg: Config = toml::from_str("[notify]\nbell = false\nmode = \"blocked+idle\"\n").unwrap();
        assert!(!cfg.notify.bell);
        assert_eq!(cfg.notify.mode, NotifyMode::BlockedIdle);
    }

    #[test]
    fn invalid_toml_falls_back_to_default() {
        let dir = std::env::temp_dir().join(format!("overseer-cfg-test-{}", uuid::Uuid::new_v4()));
        std::fs::write(&dir, b"not valid toml {{{").unwrap();
        let cfg = Config::load_from(&dir);
        assert_eq!(cfg.defaults.adapter, "claude");
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn load_from_existing_file_parses_it() {
        let dir = std::env::temp_dir().join(format!("overseer-cfg-test-{}", uuid::Uuid::new_v4()));
        std::fs::write(&dir, b"[defaults]\nadapter = \"aider\"\n").unwrap();
        let cfg = Config::load_from(&dir);
        assert_eq!(cfg.defaults.adapter, "aider");
        let _ = std::fs::remove_file(&dir);
    }
}
