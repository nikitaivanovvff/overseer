pub mod keys;
mod pty;

pub use pty::SessionManager;
// `pub` (not `pub(crate)`) because this crate's own `#[cfg(test)]` code and
// the `overseer` bin crate's tests (via a `test-util`-featured dependency on
// this crate — see AGENTS.md's terminal-backend confinement house rule) both
// need these escape-sequence-to-`GridSnapshot` render fixtures without
// importing the terminal-emulator crate themselves.
#[cfg(any(test, feature = "test-util"))]
pub use pty::{snapshot_from_bytes, snapshot_from_bytes_scrolled};
