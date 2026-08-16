use std::{
    collections::VecDeque,
    ffi::OsStr,
    fs::{File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crate::{
    path::{RepositoryRoot, ResolvedPath},
    platform::process::DetachedTree,
    tools::{
        bash::status::{
            JobSnapshot, JobState, MAX_TAIL_BYTES, RawLogSnapshot, snapshot_tail, validate_job_id,
        },
        exec::{
            ProcessError,
            capture::DRAIN_CHUNK_BYTES,
            spawn::{CLEANUP_DEADLINE, CaptureSink},
        },
    },
};

pub const DETACHED_CALLS_ENV: &str = "AGENTSHIM_DETACHED_CALLS";
pub const DEFAULT_DETACHED_CALLS: usize = 16;
pub const MAX_DETACHED_CALLS: usize = 16;
pub const TERMINAL_RETENTION: usize = 32;
const QUIESCE_SLICE: Duration = Duration::from_millis(20);

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

enum ActivePhase {
    Running(DetachedTree),
    Finalizing,
    Terminating,
}

struct ActiveJob {
    job_id: String,
    pid: u32,
    started_at: Instant,
    log_key: String,
    log_path: PathBuf,
    log_reader: Arc<File>,
    server_owned_log: bool,
    remote_drain: Option<RemoteDrain>,
    phase: ActivePhase,
    primary_exit: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalState {
    Completed,
    Terminated,
    OutcomeUncertain,
}

impl TerminalState {
    const fn model_state(self) -> JobState {
        match self {
            Self::Completed => JobState::Completed,
            Self::Terminated => JobState::Terminated,
            Self::OutcomeUncertain => JobState::OutcomeUncertain,
        }
    }
}

struct TerminalJob {
    job_id: String,
    pid: u32,
    started_at: Instant,
    finished_at: Instant,
    log_path: PathBuf,
    state: TerminalState,
    primary_exit: Option<String>,
    final_log: RawLogSnapshot,
    log_reader: Arc<File>,
    server_owned_log: bool,
}

impl Drop for TerminalJob {
    fn drop(&mut self) {
        if self.server_owned_log {
            let _ = std::fs::remove_file(&self.log_path);
        }
    }
}

#[derive(Default)]
struct Roster {
    state: RosterState,
    active: Vec<ActiveJob>,
    terminal: VecDeque<TerminalJob>,
    reserved: usize,
    reserved_paths: Vec<String>,
    reserved_job_ids: Vec<String>,
    shutdown_deadline: Option<Instant>,
    shutdown_uncertain_pids: Vec<u32>,
}

#[derive(Clone)]
pub struct DetachedTrees {
    capacity: usize,
    state: Arc<Mutex<Roster>>,
    changed: Arc<Condvar>,
    #[cfg(any(test, feature = "test-hooks"))]
    hooks: Arc<Mutex<TestHooks>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefreshOutcome {
    Stable,
    StatusUnknown,
}

impl DetachedTrees {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.clamp(1, MAX_DETACHED_CALLS),
            state: Arc::new(Mutex::new(Roster::default())),
            changed: Arc::new(Condvar::new()),
            #[cfg(any(test, feature = "test-hooks"))]
            hooks: Arc::new(Mutex::new(TestHooks::default())),
        }
    }

    pub fn admit(&self) -> Result<DetachedAdmission, ProcessError> {
        self.refresh_all_completed();
        let mut state = self.lock();
        if state.state != RosterState::Accepting {
            return Err(stopping_admission_error());
        }
        if state.active.len().saturating_add(state.reserved) >= self.capacity {
            let busy = state
                .active
                .iter()
                .map(|job| format!("pid {} -> {}", job.pid, job.log_path.display()))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ProcessError::ResourceBusy(format!(
                "all {} detached bash slots are in use: {busy}",
                self.capacity
            )));
        }
        let job_id = unique_job_id(&state);
        state.reserved = state.reserved.saturating_add(1);
        state.reserved_job_ids.push(job_id.clone());
        drop(state);
        Ok(DetachedAdmission {
            trees: self.clone(),
            job_id,
            reserved_key: None,
            settled: false,
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "commit carries the full roster transaction"
    )]
    fn commit(
        &self,
        job_id: String,
        tree: DetachedTree,
        log_path: PathBuf,
        log_reader: Arc<File>,
        reserved_key: Option<String>,
        server_owned_log: bool,
        remote_drain: Option<RemoteDrain>,
    ) -> Result<(), DetachedTree> {
        let mut state = self.lock();
        if state.state != RosterState::Accepting {
            return Err(tree);
        }
        state.reserved = state.reserved.saturating_sub(1);
        state.reserved_job_ids.retain(|held| held != &job_id);
        if let Some(key) = &reserved_key {
            state.reserved_paths.retain(|held| held != key);
        }
        let pid = tree.pid();
        state.active.push(ActiveJob {
            job_id,
            pid,
            started_at: Instant::now(),
            log_key: reserved_key.unwrap_or_default(),
            log_path,
            log_reader,
            server_owned_log,
            remote_drain,
            phase: ActivePhase::Running(tree),
            primary_exit: None,
        });
        self.changed.notify_all();
        Ok(())
    }

    fn release(&self, job_id: &str, reserved_key: Option<&str>) {
        let mut state = self.lock();
        state.reserved = state.reserved.saturating_sub(1);
        state.reserved_job_ids.retain(|held| held != job_id);
        if let Some(key) = reserved_key {
            state.reserved_paths.retain(|held| held != key);
        }
        self.changed.notify_all();
    }

    pub fn status(&self, job_id: &str, tail_bytes: usize) -> Result<JobSnapshot, ProcessError> {
        validate_job_id(job_id)?;
        let refresh = self.refresh_job(job_id);
        let source = {
            let state = self.lock();
            if let Some(job) = state.active.iter().find(|job| job.job_id == job_id) {
                let model_state = match (&job.phase, refresh) {
                    (ActivePhase::Running(_), RefreshOutcome::StatusUnknown) => {
                        JobState::StatusUnknown
                    }
                    (ActivePhase::Running(_), RefreshOutcome::Stable) => JobState::Running,
                    (ActivePhase::Finalizing, _) => JobState::Finalizing,
                    (ActivePhase::Terminating, _) => JobState::Terminating,
                };
                SnapshotSource::Active {
                    base: active_snapshot(job, model_state, None),
                    reader: Arc::clone(&job.log_reader),
                }
            } else if let Some(job) = state.terminal.iter().find(|job| job.job_id == job_id) {
                SnapshotSource::Terminal(terminal_snapshot(job, None))
            } else {
                return Err(unknown_job_error());
            }
        };
        Ok(match source {
            SnapshotSource::Active { mut base, reader } => {
                base.log = self.snapshot_log(&reader, tail_bytes.min(MAX_TAIL_BYTES));
                base
            }
            SnapshotSource::Terminal(mut snapshot) => {
                if tail_bytes < snapshot.log.bytes.len() {
                    snapshot.log.bytes = snapshot.log.bytes
                        [snapshot.log.bytes.len().saturating_sub(tail_bytes)..]
                        .to_vec();
                }
                snapshot
            }
        })
    }

    pub fn status_cursor(
        &self,
        job_id: &str,
        cursor: u64,
        max_bytes: usize,
    ) -> Result<JobSnapshot, ProcessError> {
        validate_job_id(job_id)?;
        let refresh = self.refresh_job(job_id);
        let (mut snapshot, reader, finalized) = {
            let state = self.lock();
            if let Some(job) = state.active.iter().find(|job| job.job_id == job_id) {
                let model_state = match (&job.phase, refresh) {
                    (ActivePhase::Running(_), RefreshOutcome::StatusUnknown) => {
                        JobState::StatusUnknown
                    }
                    (ActivePhase::Running(_), RefreshOutcome::Stable) => JobState::Running,
                    (ActivePhase::Finalizing, _) => JobState::Finalizing,
                    (ActivePhase::Terminating, _) => JobState::Terminating,
                };
                (
                    active_snapshot(job, model_state, None),
                    Arc::clone(&job.log_reader),
                    false,
                )
            } else if let Some(job) = state.terminal.iter().find(|job| job.job_id == job_id) {
                (
                    terminal_snapshot(job, None),
                    Arc::clone(&job.log_reader),
                    true,
                )
            } else {
                return Err(unknown_job_error());
            }
        };
        snapshot.log = crate::tools::bash::status::snapshot_from(
            &reader,
            cursor,
            max_bytes.min(MAX_TAIL_BYTES),
            finalized,
        );
        Ok(snapshot)
    }

    pub fn begin_terminate(&self, job_id: &str) -> Result<TerminateStart, ProcessError> {
        validate_job_id(job_id)?;
        let _ = self.refresh_job(job_id);
        let mut state = self.lock();
        if let Some(job) = state.terminal.iter().find(|job| job.job_id == job_id) {
            return Ok(TerminateStart::Immediate(terminal_snapshot(
                job,
                Some("already_terminal"),
            )));
        }
        let Some(index) = state.active.iter().position(|job| job.job_id == job_id) else {
            return Err(unknown_job_error());
        };
        let job = &mut state.active[index];
        match &job.phase {
            ActivePhase::Finalizing => Ok(TerminateStart::Immediate(active_snapshot(
                job,
                JobState::Finalizing,
                Some("already_completed"),
            ))),
            ActivePhase::Terminating => Ok(TerminateStart::Immediate(active_snapshot(
                job,
                JobState::Terminating,
                Some("already_requested"),
            ))),
            ActivePhase::Running(_) => {
                let phase = std::mem::replace(&mut job.phase, ActivePhase::Terminating);
                let ActivePhase::Running(tree) = phase else {
                    unreachable!("matched running phase")
                };
                let work = TerminationWork {
                    trees: self.clone(),
                    job_id: job.job_id.clone(),
                    pid: job.pid,
                    log_reader: Arc::clone(&job.log_reader),
                    primary_exit: job.primary_exit.clone(),
                    tree,
                    remote_drain: job.remote_drain.take(),
                };
                tracing::info!(target: "agentshim", event = "detached_terminate_accepted", phase = "lifecycle", outcome = "accepted", pid = job.pid);
                self.changed.notify_all();
                Ok(TerminateStart::Accepted(work))
            }
        }
    }

    pub fn begin_shutdown(&self, deadline: Instant) -> Vec<TerminationWork> {
        self.refresh_all_completed();
        let mut state = self.lock();
        state.state = RosterState::Stopping;
        state.shutdown_deadline.get_or_insert(deadline);
        let mut work = Vec::new();
        for job in &mut state.active {
            if matches!(job.phase, ActivePhase::Running(_)) {
                let phase = std::mem::replace(&mut job.phase, ActivePhase::Terminating);
                let ActivePhase::Running(tree) = phase else {
                    unreachable!("matched running phase")
                };
                work.push(TerminationWork {
                    trees: self.clone(),
                    job_id: job.job_id.clone(),
                    pid: job.pid,
                    log_reader: Arc::clone(&job.log_reader),
                    primary_exit: job.primary_exit.clone(),
                    tree,
                    remote_drain: job.remote_drain.take(),
                });
            }
        }
        self.changed.notify_all();
        work
    }

    pub fn live_tree_count(&self) -> usize {
        self.refresh_all_completed();
        let state = self.lock();
        state.active.len().saturating_add(state.reserved)
    }

    fn refresh_all_completed(&self) {
        let ids = self
            .lock()
            .active
            .iter()
            .filter(|job| matches!(job.phase, ActivePhase::Running(_)))
            .map(|job| job.job_id.clone())
            .collect::<Vec<_>>();
        for job_id in ids {
            let _ = self.refresh_job(&job_id);
        }
    }

    fn refresh_job(&self, job_id: &str) -> RefreshOutcome {
        let finalize = {
            let mut state = self.lock();
            let Some(job) = state.active.iter_mut().find(|job| job.job_id == job_id) else {
                return RefreshOutcome::Stable;
            };
            let ActivePhase::Running(tree) = &mut job.phase else {
                return RefreshOutcome::Stable;
            };
            let observation = match self.observation(tree) {
                Ok(observation) => observation,
                Err(error) => {
                    tracing::warn!(target: "agentshim", event = "detached_status_observation_degraded", phase = "lifecycle", outcome = "degraded", error_class = "io", io_kind = ?error.kind(), pid = job.pid);
                    return RefreshOutcome::StatusUnknown;
                }
            };
            if let Some(exit) = observation.primary_exit {
                job.primary_exit = Some(exit);
            }
            if observation.tree_running
                || job
                    .remote_drain
                    .as_ref()
                    .is_some_and(|drain| !drain.is_done())
            {
                return RefreshOutcome::Stable;
            }
            let phase = std::mem::replace(&mut job.phase, ActivePhase::Finalizing);
            let ActivePhase::Running(tree) = phase else {
                unreachable!("matched running phase")
            };
            drop(tree);
            Some(FinalizeWork {
                job_id: job.job_id.clone(),
                log_reader: Arc::clone(&job.log_reader),
                remote_drain: job.remote_drain.take(),
            })
        };
        if let Some(work) = finalize {
            let log = work.remote_drain.map_or_else(
                || self.snapshot_log(&work.log_reader, MAX_TAIL_BYTES),
                RemoteDrain::finish,
            );
            self.finish_terminal(&work.job_id, TerminalState::Completed, log, None);
        }
        RefreshOutcome::Stable
    }

    fn finish_terminal(
        &self,
        job_id: &str,
        terminal_state: TerminalState,
        final_log: RawLogSnapshot,
        primary_exit: Option<String>,
    ) -> JobSnapshot {
        let mut state = self.lock();
        let index = state
            .active
            .iter()
            .position(|job| job.job_id == job_id)
            .expect("terminal transition keeps its active placeholder");
        let mut active = state.active.remove(index);
        if primary_exit.is_some() {
            active.primary_exit = primary_exit;
        }
        if state.state == RosterState::Stopping
            && terminal_state == TerminalState::OutcomeUncertain
            && !state.shutdown_uncertain_pids.contains(&active.pid)
        {
            state.shutdown_uncertain_pids.push(active.pid);
        }
        let terminal = TerminalJob {
            job_id: active.job_id,
            pid: active.pid,
            started_at: active.started_at,
            finished_at: Instant::now(),
            log_path: active.log_path,
            state: terminal_state,
            primary_exit: active.primary_exit,
            final_log,
            log_reader: active.log_reader,
            server_owned_log: active.server_owned_log,
        };
        let snapshot = terminal_snapshot(&terminal, None);
        if state.terminal.len() == TERMINAL_RETENTION {
            state.terminal.pop_front();
        }
        state.terminal.push_back(terminal);
        self.changed.notify_all();
        snapshot
    }

    #[must_use]
    pub fn shutdown_deadline(&self) -> Option<Instant> {
        self.lock().shutdown_deadline
    }

    pub fn wait_until_quiesced(&self, deadline: Instant) -> bool {
        let mut state = self.lock();
        loop {
            let quiet = state.reserved == 0
                && state.reserved_paths.is_empty()
                && state.reserved_job_ids.is_empty()
                && state.active.is_empty();
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

    pub fn shutdown_unverified_pids(&self) -> Vec<u32> {
        let state = self.lock();
        let mut pids = state.shutdown_uncertain_pids.clone();
        pids.extend(
            state
                .active
                .iter()
                .filter_map(|job| matches!(job.phase, ActivePhase::Terminating).then_some(job.pid)),
        );
        pids.sort_unstable();
        pids.dedup();
        pids
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn terminate_all(&self) {
        for work in self.begin_shutdown(Instant::now() + CLEANUP_DEADLINE) {
            let _ = work.run();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Roster> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn is_accepting(&self) -> bool {
        matches!(self.lock().state, RosterState::Accepting)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn live_count(&self) -> usize {
        self.lock().active.len()
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn terminal_count(&self) -> usize {
        self.lock().terminal.len()
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn reserved_count(&self) -> usize {
        self.lock().reserved
    }

    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn observation(
        &self,
        tree: &mut DetachedTree,
    ) -> io::Result<crate::platform::process::DetachedObservation> {
        #[cfg(any(test, feature = "test-hooks"))]
        if std::mem::take(&mut self.lock_hooks().fail_liveness_query) {
            return Err(io::Error::other(
                "injected degraded liveness query for lifecycle testing",
            ));
        }
        tree.observe()
    }

    #[cfg_attr(
        not(test),
        allow(
            clippy::unused_self,
            reason = "test builds inject registry-owned log failures"
        )
    )]
    fn snapshot_log(&self, file: &File, tail_bytes: usize) -> RawLogSnapshot {
        #[cfg(any(test, feature = "test-hooks"))]
        if std::mem::take(&mut self.lock_hooks().fail_tail_snapshot) {
            return RawLogSnapshot::empty_with_error("injected detached log read failure");
        }
        snapshot_tail(file, tail_bytes)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn set_before_open_hook(&self, hook: impl FnOnce() + Send + 'static) {
        self.lock_hooks().before_open = Some(Box::new(hook));
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn set_after_open_hook(&self, hook: impl FnOnce() + Send + 'static) {
        self.lock_hooks().after_open = Some(Box::new(hook));
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn set_after_commit_hook(&self, hook: impl FnOnce() + Send + 'static) {
        self.lock_hooks().after_commit = Some(Box::new(hook));
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn fail_next_spawn(&self) {
        self.lock_hooks().fail_spawn = true;
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn fail_next_liveness_query(&self) {
        self.lock_hooks().fail_liveness_query = true;
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn fail_next_tail_snapshot(&self) {
        self.lock_hooks().fail_tail_snapshot = true;
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn fail_next_termination(&self) {
        self.lock_hooks().fail_termination = true;
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn take_termination_failure(&self) -> bool {
        std::mem::take(&mut self.lock_hooks().fail_termination)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn run_before_open_hook(&self) {
        if let Some(hook) = self.lock_hooks().before_open.take() {
            hook();
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn run_after_open_hook(&self) {
        if let Some(hook) = self.lock_hooks().after_open.take() {
            hook();
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn run_after_commit_hook(&self) {
        if let Some(hook) = self.lock_hooks().after_commit.take() {
            hook();
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn take_spawn_failure(&self) -> bool {
        std::mem::take(&mut self.lock_hooks().fail_spawn)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn lock_hooks(&self) -> std::sync::MutexGuard<'_, TestHooks> {
        self.hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

enum SnapshotSource {
    Active {
        base: JobSnapshot,
        reader: Arc<File>,
    },
    Terminal(JobSnapshot),
}

struct FinalizeWork {
    job_id: String,
    log_reader: Arc<File>,
    remote_drain: Option<RemoteDrain>,
}

pub enum TerminateStart {
    Immediate(JobSnapshot),
    Accepted(TerminationWork),
}

pub struct TerminationWork {
    trees: DetachedTrees,
    job_id: String,
    pid: u32,
    log_reader: Arc<File>,
    primary_exit: Option<String>,
    tree: DetachedTree,
    remote_drain: Option<RemoteDrain>,
}

impl TerminationWork {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn run(mut self) -> JobSnapshot {
        let deadline = self.trees.shutdown_deadline().map_or_else(
            || Instant::now() + CLEANUP_DEADLINE,
            |shutdown| shutdown.min(Instant::now() + CLEANUP_DEADLINE),
        );
        #[cfg(any(test, feature = "test-hooks"))]
        let injected_failure = self.trees.take_termination_failure();
        #[cfg(not(any(test, feature = "test-hooks")))]
        let injected_failure = false;
        let terminal_state = if injected_failure {
            TerminalState::OutcomeUncertain
        } else {
            match self.tree.terminate_and_wait(deadline) {
                Ok(()) => TerminalState::Terminated,
                Err(_) => TerminalState::OutcomeUncertain,
            }
        };
        if let Ok(observation) = self.tree.observe()
            && observation.primary_exit.is_some()
        {
            self.primary_exit = observation.primary_exit;
        }
        let final_log = self.remote_drain.map_or_else(
            || self.trees.snapshot_log(&self.log_reader, MAX_TAIL_BYTES),
            RemoteDrain::finish,
        );
        let state = terminal_state.model_state().label();
        let snapshot =
            self.trees
                .finish_terminal(&self.job_id, terminal_state, final_log, self.primary_exit);
        tracing::info!(target: "agentshim", event = "detached_terminate_finished", phase = "lifecycle", outcome = state, pid = self.pid);
        snapshot
    }
}

fn active_snapshot(job: &ActiveJob, state: JobState, outcome: Option<&'static str>) -> JobSnapshot {
    JobSnapshot {
        job_id: job.job_id.clone(),
        state,
        pid: job.pid,
        runtime: job.started_at.elapsed(),
        primary_exit: job.primary_exit.clone(),
        log_path: job.log_path.clone(),
        log: RawLogSnapshot {
            total: 0,
            start: 0,
            bytes: Vec::new(),
            error: None,
        },
        outcome,
    }
}

fn terminal_snapshot(job: &TerminalJob, outcome: Option<&'static str>) -> JobSnapshot {
    JobSnapshot {
        job_id: job.job_id.clone(),
        state: job.state.model_state(),
        pid: job.pid,
        runtime: job.finished_at.saturating_duration_since(job.started_at),
        primary_exit: job.primary_exit.clone(),
        log_path: job.log_path.clone(),
        log: job.final_log.clone(),
        outcome,
    }
}

fn unique_job_id(state: &Roster) -> String {
    loop {
        let candidate = format!("bash-{}", uuid::Uuid::new_v4());
        let used = state.active.iter().any(|job| job.job_id == candidate)
            || state.terminal.iter().any(|job| job.job_id == candidate)
            || state.reserved_job_ids.iter().any(|held| held == &candidate);
        if !used {
            return candidate;
        }
    }
}

fn unknown_job_error() -> ProcessError {
    ProcessError::Validation("unknown or expired bash job_id for this server instance".to_owned())
}

fn stopping_admission_error() -> ProcessError {
    ProcessError::ResourceBusy(
        "detached bash is no longer admitting: the server is stopping".to_owned(),
    )
}

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

#[cfg(any(test, feature = "test-hooks"))]
#[derive(Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent one-shot lifecycle failure hooks keep tests deterministic"
)]
struct TestHooks {
    before_open: Option<Box<dyn FnOnce() + Send>>,
    after_open: Option<Box<dyn FnOnce() + Send>>,
    after_commit: Option<Box<dyn FnOnce() + Send>>,
    fail_spawn: bool,
    fail_liveness_query: bool,
    fail_tail_snapshot: bool,
    fail_termination: bool,
}

pub struct DetachedAdmission {
    trees: DetachedTrees,
    job_id: String,
    reserved_key: Option<String>,
    settled: bool,
}

impl DetachedAdmission {
    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    pub fn reserve_log_path(&mut self, path: &Path) -> Result<(), ProcessError> {
        let key = normalized_log_key(path);
        let mut state = self.trees.lock();
        if state.state != RosterState::Accepting {
            return Err(stopping_admission_error());
        }
        if state.reserved_paths.iter().any(|held| held == &key)
            || state.active.iter().any(|job| job.log_key == key)
        {
            return Err(ProcessError::ResourceBusy(format!(
                "log_path {} is already in use by an active or reserved detached call in this instance",
                path.display()
            )));
        }
        state.reserved_paths.push(key.clone());
        self.reserved_key = Some(key);
        Ok(())
    }

    pub fn retain(
        mut self,
        tree: DetachedTree,
        log_path: PathBuf,
        log_reader: Arc<File>,
        server_owned_log: bool,
    ) -> Result<(), DetachedTree> {
        match self.trees.commit(
            self.job_id.clone(),
            tree,
            log_path,
            log_reader,
            self.reserved_key.clone(),
            server_owned_log,
            None,
        ) {
            Ok(()) => {
                self.settled = true;
                #[cfg(any(test, feature = "test-hooks"))]
                self.trees.run_after_commit_hook();
                Ok(())
            }
            Err(tree) => Err(tree),
        }
    }

    pub fn retain_remote(
        mut self,
        tree: DetachedTree,
        log_reader: Arc<File>,
        remote_drain: RemoteDrain,
    ) -> Result<(), DetachedTree> {
        match self.trees.commit(
            self.job_id.clone(),
            tree,
            PathBuf::from("remote-capture"),
            log_reader,
            self.reserved_key.clone(),
            false,
            Some(remote_drain),
        ) {
            Ok(()) => {
                self.settled = true;
                Ok(())
            }
            Err(tree) => Err(tree),
        }
    }

    #[must_use]
    pub fn rollback_deadline(&self) -> Instant {
        let budget = self
            .trees
            .shutdown_deadline()
            .map_or(CLEANUP_DEADLINE, |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(CLEANUP_DEADLINE)
            });
        Instant::now() + budget
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn before_open(&self) {
        self.trees.run_before_open_hook();
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn after_open(&self) {
        self.trees.run_after_open_hook();
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn injected_spawn_error(&self) -> Option<ProcessError> {
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
            self.trees.release(&self.job_id, key.as_deref());
        }
    }
}

pub struct DetachedLog {
    pub writer: File,
    pub reader: Arc<File>,
}

pub fn open_log(root: &RepositoryRoot, path: &ResolvedPath) -> Result<DetachedLog, ProcessError> {
    let writer = root.create_truncated(path).map_err(|error| {
        ProcessError::Validation(format!(
            "cannot open log_path {}: {error}",
            path.absolute().display()
        ))
    })?;
    let reader = Arc::new(writer.try_clone().map_err(ProcessError::Io)?);
    Ok(DetachedLog { writer, reader })
}

pub fn server_log_path(job_id: &str) -> PathBuf {
    std::env::temp_dir()
        .join("agentshim-dsh-capture")
        .join(format!("{job_id}.log"))
}

pub fn open_server_log(path: &Path) -> Result<DetachedLog, ProcessError> {
    let directory = path
        .parent()
        .ok_or_else(|| ProcessError::Io(io::Error::other("server log has no parent")))?;
    std::fs::create_dir_all(directory).map_err(ProcessError::Io)?;
    let writer = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(ProcessError::Io)?;
    let reader = Arc::new(writer.try_clone().map_err(ProcessError::Io)?);
    Ok(DetachedLog { writer, reader })
}

struct RemoteDrainState {
    done: bool,
    total: u64,
    error: Option<String>,
}

pub struct RemoteDrain {
    state: Arc<Mutex<RemoteDrainState>>,
    handle: Option<JoinHandle<()>>,
}

impl RemoteDrain {
    fn is_done(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .done
    }

    fn finish(mut self) -> RawLogSnapshot {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        RawLogSnapshot {
            total: state.total,
            start: state.total,
            bytes: Vec::new(),
            error: state.error.clone(),
        }
    }
}

pub fn start_remote_drain(mut reader: File, sink: Arc<dyn CaptureSink>) -> RemoteDrain {
    let state = Arc::new(Mutex::new(RemoteDrainState {
        done: false,
        total: 0,
        error: None,
    }));
    let thread_state = Arc::clone(&state);
    let handle = std::thread::spawn(move || {
        let mut buffer = vec![0_u8; DRAIN_CHUNK_BYTES];
        let result = loop {
            match reader.read(&mut buffer) {
                Ok(0) => break Ok(()),
                Ok(read) => {
                    if let Err(error) = sink.append(0, &buffer[..read]) {
                        break Err(error);
                    }
                    let mut state = thread_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.total = state
                        .total
                        .saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
                }
                Err(error) => break Err(error),
            }
        };
        let error = result.as_ref().err().map(ToString::to_string);
        let _ = sink.complete(result.is_ok(), error.as_deref());
        let mut state = thread_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.error = error;
        state.done = true;
    });
    RemoteDrain {
        state,
        handle: Some(handle),
    }
}

pub fn empty_log_reader() -> Result<Arc<File>, ProcessError> {
    #[cfg(windows)]
    let file = File::open("NUL")?;
    #[cfg(unix)]
    let file = File::open("/dev/null")?;
    Ok(Arc::new(file))
}
