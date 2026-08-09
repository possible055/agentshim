use std::{
    ffi::OsStr,
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use crate::{
    path::{RepositoryRoot, ResolvedPath},
    tools::exec::{ProcessError, platform::DetachedTree},
};

pub(crate) const DETACHED_CALLS_ENV: &str = "CODEXSHIM_DETACHED_CALLS";
pub(crate) const DEFAULT_DETACHED_CALLS: usize = 16;
pub(crate) const MAX_DETACHED_CALLS: usize = 16;

/// Parse the detached-tree capacity.
///
/// # Errors
///
/// Returns invalid input when the value is not an integer inside the documented range, so an
/// invalid setting fails startup rather than a task.
pub fn parse_detached_calls(value: Option<&OsStr>) -> io::Result<usize> {
    match value {
        None => Ok(DEFAULT_DETACHED_CALLS),
        Some(value) => value
            .to_str()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| (1..=MAX_DETACHED_CALLS).contains(value))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{DETACHED_CALLS_ENV} must be an integer from 1 to {MAX_DETACHED_CALLS}"
                    ),
                )
            }),
    }
}

struct Slot {
    tree: DetachedTree,
    log_path: PathBuf,
}

/// Fixed-capacity ownership of detached process trees, scanned at admission rather than by a
/// reaper thread: a finished tree frees its slot on the next detached call, which is the only
/// moment the count matters.
#[derive(Clone)]
pub(crate) struct DetachedTrees {
    capacity: usize,
    state: Arc<Mutex<Roster>>,
    #[cfg(test)]
    hooks: Arc<Mutex<TestHooks>>,
}

#[derive(Default)]
struct Roster {
    live: Vec<Slot>,
    /// Admissions that have not yet produced a tree. Counted alongside `live` so two callers
    /// cannot both observe the same free slot between admission and spawn.
    reserved: usize,
}

impl DetachedTrees {
    #[must_use]
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.clamp(1, MAX_DETACHED_CALLS),
            state: Arc::new(Mutex::new(Roster::default())),
            #[cfg(test)]
            hooks: Arc::new(Mutex::new(TestHooks::default())),
        }
    }

    pub(crate) fn admit(&self) -> Result<DetachedAdmission, ProcessError> {
        let mut state = self.lock();
        state.live.retain_mut(|slot| slot.tree.is_running());
        if state.live.len().saturating_add(state.reserved) >= self.capacity {
            let busy = state
                .live
                .iter()
                .map(|slot| format!("pid {} -> {}", slot.tree.pid(), slot.log_path.display()))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ProcessError::ResourceBusy(format!(
                "all {} detached bash slots are in use: {busy}",
                self.capacity
            )));
        }
        state.reserved = state.reserved.saturating_add(1);
        drop(state);
        Ok(DetachedAdmission {
            trees: self.clone(),
            settled: false,
        })
    }

    /// Convert a reservation into a live slot without leaving the count low in between.
    fn commit(&self, tree: DetachedTree, log_path: PathBuf) {
        let mut state = self.lock();
        state.reserved = state.reserved.saturating_sub(1);
        state.live.push(Slot { tree, log_path });
    }

    fn release(&self) {
        let mut state = self.lock();
        state.reserved = state.reserved.saturating_sub(1);
    }

    pub(crate) fn terminate_all(&self) {
        let mut state = self.lock();
        for slot in &mut state.live {
            slot.tree.terminate();
        }
        state.live.clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Roster> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn live_count(&self) -> usize {
        self.lock().live.len()
    }

    #[cfg(test)]
    pub(crate) fn reserved_count(&self) -> usize {
        self.lock().reserved
    }

    #[cfg(test)]
    pub(crate) fn set_before_open_hook(&self, hook: impl FnOnce() + Send + 'static) {
        self.lock_hooks().before_open = Some(Box::new(hook));
    }

    #[cfg(test)]
    pub(crate) fn set_after_open_hook(&self, hook: impl FnOnce() + Send + 'static) {
        self.lock_hooks().after_open = Some(Box::new(hook));
    }

    #[cfg(test)]
    pub(crate) fn fail_next_spawn(&self) {
        self.lock_hooks().fail_spawn = true;
    }

    #[cfg(test)]
    fn run_before_open_hook(&self) {
        let hook = self.lock_hooks().before_open.take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(test)]
    fn run_after_open_hook(&self) {
        let hook = self.lock_hooks().after_open.take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(test)]
    fn take_spawn_failure(&self) -> bool {
        let mut hooks = self.lock_hooks();
        std::mem::take(&mut hooks.fail_spawn)
    }

    #[cfg(test)]
    fn lock_hooks(&self) -> std::sync::MutexGuard<'_, TestHooks> {
        self.hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[derive(Default)]
struct TestHooks {
    before_open: Option<Box<dyn FnOnce() + Send>>,
    after_open: Option<Box<dyn FnOnce() + Send>>,
    fail_spawn: bool,
}

/// A slot held from admission until a tree fills it. Dropping without [`Self::retain`] — a
/// cancelled request, a failed spawn, a `?` on any step in between — returns the slot, so the
/// capacity is never leaked and never double-issued.
pub(crate) struct DetachedAdmission {
    trees: DetachedTrees,
    settled: bool,
}

impl DetachedAdmission {
    pub(crate) fn retain(mut self, tree: DetachedTree, log_path: PathBuf) {
        self.trees.commit(tree, log_path);
        self.settled = true;
    }

    #[cfg(test)]
    pub(crate) fn before_open(&self) {
        self.trees.run_before_open_hook();
    }

    #[cfg(test)]
    pub(crate) fn after_open(&self) {
        self.trees.run_after_open_hook();
    }

    #[cfg(test)]
    pub(crate) fn injected_spawn_error(&self) -> Option<ProcessError> {
        self.trees.take_spawn_failure().then(|| {
            ProcessError::Io(io::Error::other(
                "injected detached spawn failure for lifecycle testing",
            ))
        })
    }
}

impl Drop for DetachedAdmission {
    fn drop(&mut self) {
        if !self.settled {
            self.trees.release();
        }
    }
}

/// Open the log in truncating mode through the repository capability, never through an ambient
/// path. [`RepositoryRoot::resolve`] is lexical admission only, so a symlink or junction that
/// lives inside the repository but points outside it passes that check; going through the
/// retained directory handle is what actually keeps the write inside the root.
///
/// The parent directory must already exist: creating it implicitly would let a typo silently
/// scatter files through the repository.
pub(crate) fn open_log(
    root: &RepositoryRoot,
    path: &ResolvedPath,
) -> Result<std::fs::File, ProcessError> {
    root.create_truncated(path).map_err(|error| {
        ProcessError::Validation(format!(
            "cannot open log_path {}: {error}",
            path.absolute().display()
        ))
    })
}
