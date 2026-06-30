use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

pub struct GitClient {
    dry_run: bool,
}

impl GitClient {
    pub fn new() -> Self {
        Self { dry_run: false }
    }

    /// Returns a no-op client that returns fixed test values without invoking git.
    #[cfg(test)]
    pub fn dry_run() -> Self {
        Self { dry_run: true }
    }

    /// Returns the repository name (last path segment of the git root).
    pub fn repo_name(&self, cwd: &Path) -> Result<String> {
        if self.dry_run {
            return Ok("test-repo".to_string());
        }
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(cwd)
            .output()
            .context("failed to run git")?;
        anyhow::ensure!(output.status.success(), "git rev-parse --show-toplevel failed");
        let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let name = Path::new(&root)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(root);
        Ok(name)
    }

    /// Returns the current branch name (e.g. "main").
    pub fn current_branch(&self, cwd: &Path) -> Result<String> {
        if self.dry_run {
            return Ok("test-branch".to_string());
        }
        let output = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(cwd)
            .output()
            .context("failed to run git")?;
        anyhow::ensure!(output.status.success(), "git rev-parse --abbrev-ref HEAD failed");
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

impl Default for GitClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_repo_name() {
        let g = GitClient::dry_run();
        assert_eq!(g.repo_name(Path::new("/any/path")).unwrap(), "test-repo");
    }

    #[test]
    fn dry_run_current_branch() {
        let g = GitClient::dry_run();
        assert_eq!(g.current_branch(Path::new("/any/path")).unwrap(), "test-branch");
    }
}
