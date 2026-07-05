//! Minimal `[defaults]`/`[adapters.*]` config loader (AGENTS.md "Config").
//! Keybindings/theme are still Phase 5b — not modeled here.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default = "default_adapters")]
    pub adapters: HashMap<String, AdapterConfig>,
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
        Self { defaults: Defaults::default(), adapters: default_adapters() }
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
