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
    key: &'static str,
    prior: Option<String>,
}

impl EnvGuard {
    pub(crate) fn set(key: &'static str, value: &str) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { _lock: lock, key, prior }
    }

    pub(crate) fn unset(key: &'static str) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(key).ok();
        std::env::remove_var(key);
        Self { _lock: lock, key, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}
