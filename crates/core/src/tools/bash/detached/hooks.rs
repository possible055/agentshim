//! Test-only lifecycle hooks: deterministic injection points for spawn,
//! liveness, log reads, and termination failures. Gated behind
//! `cfg(any(test, feature = "test-hooks"))`; the root crate's server tests
//! enable the feature from outside, so this surface is real there.

use std::sync::{Arc, Mutex};

use super::{DetachedTrees, RosterState};
use crate::tools::exec::spawn::CLEANUP_DEADLINE;

#[derive(Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent one-shot lifecycle failure hooks keep tests deterministic"
)]
pub(super) struct TestHooks {
    pub(super) before_open: Option<Box<dyn FnOnce() + Send>>,
    pub(super) after_open: Option<Box<dyn FnOnce() + Send>>,
    pub(super) after_commit: Option<Box<dyn FnOnce() + Send>>,
    pub(super) fail_spawn: bool,
    pub(super) fail_liveness_query: bool,
    pub(super) fail_tail_snapshot: bool,
    pub(super) fail_termination: bool,
}

pub(super) type SharedHooks = Arc<Mutex<TestHooks>>;

pub(super) fn shared_default() -> SharedHooks {
    Arc::new(Mutex::new(TestHooks::default()))
}

pub(super) fn lock(hooks: &SharedHooks) -> std::sync::MutexGuard<'_, TestHooks> {
    hooks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl DetachedTrees {
    pub fn terminate_all(&self) {
        for work in self.begin_shutdown(std::time::Instant::now() + CLEANUP_DEADLINE) {
            let _ = work.run();
        }
    }

    pub fn is_accepting(&self) -> bool {
        matches!(self.lock().state, RosterState::Accepting)
    }

    pub fn live_count(&self) -> usize {
        self.lock().active.len()
    }

    pub fn terminal_count(&self) -> usize {
        self.lock().terminal.len()
    }

    pub fn reserved_count(&self) -> usize {
        self.lock().reserved
    }

    pub fn set_before_open_hook(&self, hook: impl FnOnce() + Send + 'static) {
        lock(&self.hooks).before_open = Some(Box::new(hook));
    }

    pub fn set_after_open_hook(&self, hook: impl FnOnce() + Send + 'static) {
        lock(&self.hooks).after_open = Some(Box::new(hook));
    }

    pub fn set_after_commit_hook(&self, hook: impl FnOnce() + Send + 'static) {
        lock(&self.hooks).after_commit = Some(Box::new(hook));
    }

    pub fn fail_next_spawn(&self) {
        lock(&self.hooks).fail_spawn = true;
    }

    pub fn fail_next_liveness_query(&self) {
        lock(&self.hooks).fail_liveness_query = true;
    }

    pub fn fail_next_tail_snapshot(&self) {
        lock(&self.hooks).fail_tail_snapshot = true;
    }

    pub fn fail_next_termination(&self) {
        lock(&self.hooks).fail_termination = true;
    }

    pub(super) fn take_termination_failure(&self) -> bool {
        std::mem::take(&mut lock(&self.hooks).fail_termination)
    }

    pub(super) fn run_before_open_hook(&self) {
        if let Some(hook) = lock(&self.hooks).before_open.take() {
            hook();
        }
    }

    pub(super) fn run_after_open_hook(&self) {
        if let Some(hook) = lock(&self.hooks).after_open.take() {
            hook();
        }
    }

    pub(super) fn run_after_commit_hook(&self) {
        if let Some(hook) = lock(&self.hooks).after_commit.take() {
            hook();
        }
    }

    pub(super) fn take_spawn_failure(&self) -> bool {
        std::mem::take(&mut lock(&self.hooks).fail_spawn)
    }
}
