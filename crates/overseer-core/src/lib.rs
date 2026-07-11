//! `overseer-core` — the client-agnostic core of Overseer: agent model +
//! registry, session/PTY management, the IPC protocol + server, the daemon
//! process, git info, and config parsing. No UI toolkit lives here — see
//! AGENTS.md's "Architecture" section for the full module breakdown and the
//! `overseer` binary crate for the TUI that consumes this library.

pub mod agent;
pub mod config;
pub mod daemon;
pub mod git;
pub mod install;
pub mod ipc;
pub mod kill;
pub mod notify;
pub mod session;
pub mod settings;

/// Shared test-only helper for mutating process-global env vars, plus
/// (behind the same gate) the escape-sequence-to-`GridSnapshot` render
/// helpers `session::pty` exposes for tests. Available to this crate's own
/// `#[cfg(test)]` code unconditionally, and to other workspace crates' tests
/// when they depend on `overseer-core` with `features = ["test-util"]` — see
/// `crates/overseer/Cargo.toml`'s `[dev-dependencies]`.
#[cfg(any(test, feature = "test-util"))]
pub mod test_env;
