//! Shared test-only helper for mutating process-global env vars.
//!
//! `cli.rs`'s `OVERSEER_AGENT_ID` tests and `agent/spawn.rs`'s `SHELL` tests
//! each set/remove a real env var, which races any other test reading the
//! same var under the parallel test runner. `EnvGuard` serializes all such
//! tests behind one lock and restores the prior value (or absence) on drop.

use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

pub(crate) struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    // (key, prior value) pairs to restore on drop. A plain `MutexGuard` can't
    // be acquired twice on the same thread (std's `Mutex` isn't reentrant),
    // so a test needing several vars set atomically (e.g. every adapter's
    // config-dir env var at once) must go through `set_all` rather than
    // holding multiple single-key `EnvGuard`s side by side.
    restores: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    pub(crate) fn set(key: &'static str, value: &str) -> Self {
        Self::set_all(&[(key, value)])
    }

    pub(crate) fn unset(key: &'static str) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(key).ok();
        std::env::remove_var(key);
        Self { _lock: lock, restores: vec![(key, prior)] }
    }

    /// Sets every `(key, value)` pair under one lock acquisition, restoring
    /// all of them on drop — the multi-var counterpart to `set`.
    pub(crate) fn set_all(pairs: &[(&'static str, &str)]) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let restores = pairs
            .iter()
            .map(|(key, value)| {
                let prior = std::env::var(key).ok();
                std::env::set_var(key, value);
                (*key, prior)
            })
            .collect();
        Self { _lock: lock, restores }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, prior) in &self.restores {
            match prior {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
