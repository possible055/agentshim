use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use crate::{
    path::{RepositoryRoot, ResolvedPath},
    platform::process::DetachedTree,
    tools::exec::{ProcessError, spawn::CLEANUP_DEADLINE},
};

pub(crate) const DETACHED_CALLS_ENV: &str = "CODEXSHIM_DETACHED_CALLS";
pub(crate) const DEFAULT_DETACHED_CALLS: usize = 16;
pub(crate) const MAX_DETACHED_CALLS: usize = 16;
const QUIESCE_SLICE: Duration = Duration::from_millis(20);

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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RosterState {
    #[default]
    Accepting,
    Stopping,
}

struct Slot {
    tree: DetachedTree,
    log_key: String,
    log_path: PathBuf,
}

/// Fixed-capacity ownership of detached process trees, scanned at admission rather than by a
/// reaper thread: a finished tree frees its slot on the next detached call, which is the only
/// moment the count matters. Reservations and live slots together hold normalized
/// `log_path` keys, so two calls in one instance cannot share an observation pipe; that
/// uniqueness is an instance-local protection, not an enumerable or persistent registry.
#[derive(Clone)]
pub(crate) struct DetachedTrees {
    capacity: usize,
    state: Arc<Mutex<Roster>>,
    changed: Arc<Condvar>,
    #[cfg(test)]
    hooks: Arc<Mutex<TestHooks>>,
}

#[derive(Default)]
struct Roster {
    state: RosterState,
    live: Vec<Slot>,
    /// Admissions that have not yet produced a tree. Counted alongside `live` so two callers
    /// cannot both observe the same free slot between admission and spawn.
    reserved: usize,
    /// Normalized `log_path` keys held by admissions that resolved their log.
    reserved_paths: Vec<String>,
    shutdown_deadline: Option<Instant>,
}

impl DetachedTrees {
    #[must_use]
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.clamp(1, MAX_DETACHED_CALLS),
            state: Arc::new(Mutex::new(Roster::default())),
            changed: Arc::new(Condvar::new()),
            #[cfg(test)]
            hooks: Arc::new(Mutex::new(TestHooks::default())),
        }
    }

    pub(crate) fn admit(&self) -> Result<DetachedAdmission, ProcessError> {
        let mut state = self.lock();
        if state.state != RosterState::Accepting {
            return Err(stopping_admission_error());
        }
        state
            .live
            .retain_mut(|slot| match self.liveness(&mut slot.tree) {
                Ok(running) => running,
                Err(error) => {
                    // A failed liveness query is a degraded observation, not an exit report:
                    // dropping the slot would close the only job handle and kill a tree that
                    // may still be running, so the owner and its capacity stay booked.
                    tracing::warn!(
                        target: "codexshim",
                        event = "detached_liveness_degraded",
                        phase = "execution",
                        outcome = "degraded",
                        error_class = "io",
                        io_kind = ?error.kind(),
                        pid = slot.tree.pid()
                    );
                    true
                }
            });
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
            reserved_key: None,
            settled: false,
        })
    }

    /// Convert a reservation into a live slot without leaving the count low in between.
    /// A roster that stopped accepting hands the tree back: the caller — not the roster —
    /// must roll it back, so the owner can never be lost on an error path.
    fn commit(
        &self,
        tree: DetachedTree,
        log_path: PathBuf,
        reserved_key: Option<String>,
    ) -> Result<(), DetachedTree> {
        let mut state = self.lock();
        if state.state != RosterState::Accepting {
            return Err(tree);
        }
        state.reserved = state.reserved.saturating_sub(1);
        if let Some(key) = &reserved_key {
            state.reserved_paths.retain(|held| held != key);
        }
        state.live.push(Slot {
            tree,
            log_key: reserved_key.unwrap_or_default(),
            log_path,
        });
        self.changed.notify_all();
        Ok(())
    }

    fn release(&self, reserved_key: Option<&str>) {
        let mut state = self.lock();
        state.reserved = state.reserved.saturating_sub(1);
        if let Some(key) = reserved_key {
            state.reserved_paths.retain(|held| held != key);
        }
        self.changed.notify_all();
    }

    /// Enter `Stopping` and hand out every committed tree for termination outside the
    /// roster lock. Idempotent: overlapping shutdown triggers share one transition.
    pub(crate) fn begin_shutdown(&self) -> Vec<DetachedTree> {
        let mut state = self.lock();
        state.state = RosterState::Stopping;
        state.shutdown_deadline = Some(Instant::now() + CLEANUP_DEADLINE);
        self.changed.notify_all();
        state.live.drain(..).map(|slot| slot.tree).collect()
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn is_accepting(&self) -> bool {
        matches!(self.lock().state, RosterState::Accepting)
    }

    /// The shared shutdown deadline, if this roster has stopped accepting.
    #[must_use]
    pub(crate) fn shutdown_deadline(&self) -> Option<Instant> {
        self.lock().shutdown_deadline
    }

    /// Block until no reservation or live tree remains, bounded by `deadline`. Returns
    /// whether the roster actually reached zero.
    pub(crate) fn wait_until_quiesced(&self, deadline: Instant) -> bool {
        let mut state = self.lock();
        loop {
            let quiet =
                state.reserved == 0 && state.reserved_paths.is_empty() && state.live.is_empty();
            if quiet || Instant::now() >= deadline {
                return quiet;
            }
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(QUIESCE_SLICE);
            let (guard, _) = self
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = guard;
        }
    }

    /// Test-only emergency sweep: production shutdown goes through the single
    /// `shutdown_processes` transaction, which also waits for quiescence.
    #[cfg(test)]
    pub(crate) fn terminate_all(&self) {
        let mut state = self.lock();
        for slot in &mut state.live {
            slot.tree.terminate();
        }
        state.state = RosterState::Stopping;
        state.live.clear();
        state.reserved_paths.clear();
        self.changed.notify_all();
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

    // Only the test build reaches the hook through `self`; the production body is a plain
    // forward and keeps the receiver only to mirror the test signature.
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn liveness(&self, tree: &mut DetachedTree) -> io::Result<bool> {
        #[cfg(test)]
        if std::mem::take(&mut self.lock_hooks().fail_liveness_query) {
            return Err(io::Error::other(
                "injected degraded liveness query for lifecycle testing",
            ));
        }
        tree.is_running()
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
    pub(crate) fn fail_next_liveness_query(&self) {
        self.lock_hooks().fail_liveness_query = true;
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

fn stopping_admission_error() -> ProcessError {
    ProcessError::ResourceBusy(
        "detached bash is no longer admitting: the server is stopping".to_owned(),
    )
}

/// Windows matches `log_path` keys case-insensitively and treats `/` and `\` as the same
/// separator; Unix keeps the platform's native case semantics.
fn normalized_log_key(path: &Path) -> String {
    let rendered = path.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        rendered.to_ascii_lowercase().replace('/', "\\")
    }
    #[cfg(not(windows))]
    {
        rendered
    }
}

#[cfg(test)]
#[derive(Default)]
struct TestHooks {
    before_open: Option<Box<dyn FnOnce() + Send>>,
    after_open: Option<Box<dyn FnOnce() + Send>>,
    fail_spawn: bool,
    fail_liveness_query: bool,
}

/// A slot held from admission until a tree fills it. Dropping without [`Self::retain`] — a
/// cancelled request, a failed spawn, a `?` on any step in between — returns the slot and
/// its `log_path` key, so the capacity is never leaked and never double-issued.
pub(crate) struct DetachedAdmission {
    trees: DetachedTrees,
    reserved_key: Option<String>,
    settled: bool,
}

impl DetachedAdmission {
    /// Reserve the normalized `log_path` key against live slots and other reservations.
    /// Called before the log is truncated, so a duplicate never destroys an existing log.
    ///
    /// # Errors
    ///
    /// Returns a resource conflict when the path is already in use, or busy when the
    /// roster has stopped accepting.
    pub(crate) fn reserve_log_path(&mut self, path: &Path) -> Result<(), ProcessError> {
        let key = normalized_log_key(path);
        let mut state = self.trees.lock();
        if state.state != RosterState::Accepting {
            return Err(stopping_admission_error());
        }
        if state.reserved_paths.iter().any(|held| held == &key)
            || state.live.iter().any(|slot| slot.log_key == key)
        {
            return Err(ProcessError::ResourceBusy(format!(
                "log_path {} is already in use by an active or reserved detached call in \
                 this instance",
                path.display()
            )));
        }
        state.reserved_paths.push(key.clone());
        self.reserved_key = Some(key);
        Ok(())
    }

    /// Commit the tree. On rejection the tree is returned to the caller, which must
    /// terminate it: user code may already have run.
    pub(crate) fn retain(
        mut self,
        tree: DetachedTree,
        log_path: PathBuf,
    ) -> Result<(), DetachedTree> {
        match self.trees.commit(tree, log_path, self.reserved_key.clone()) {
            Ok(()) => {
                self.settled = true;
                Ok(())
            }
            Err(tree) => Err(tree),
        }
    }

    /// A late rollback shares the roster's shutdown deadline when one is running, so a
    /// shutdown never waits longer than its own single budget for a straggler call.
    #[must_use]
    pub(crate) fn rollback_deadline(&self) -> Instant {
        let budget = match self.trees.shutdown_deadline() {
            Some(deadline) => deadline
                .saturating_duration_since(Instant::now())
                .min(CLEANUP_DEADLINE),
            None => CLEANUP_DEADLINE,
        };
        Instant::now() + budget
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
            let key = self.reserved_key.take();
            self.trees.release(key.as_deref());
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
