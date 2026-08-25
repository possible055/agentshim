use std::{
    collections::VecDeque,
    ffi::OsStr,
    fs::File,
    io,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use crate::{
    platform::process::DetachedTree,
    tools::{
        bash::status::{
            JobSnapshot, JobState, MAX_TAIL_BYTES, RawLogSnapshot, snapshot_tail, validate_job_id,
        },
        exec::{ProcessError, spawn::CLEANUP_DEADLINE},
    },
};
use tokio_util::sync::CancellationToken;

mod admission;
#[cfg(any(test, feature = "test-hooks"))]
mod hooks;
mod launch;

pub use admission::{DetachedAdmission, DetachedLog, open_log};
#[cfg(any(test, feature = "test-hooks"))]
use hooks::SharedHooks;
pub(in crate::tools::bash) use launch::execute_detached;

pub const DETACHED_CALLS_ENV: &str = "AGENTSHIM_DETACHED_CALLS";
pub const DETACHED_LOG_BYTES_ENV: &str = "AGENTSHIM_DETACHED_LOG_BYTES";
pub const DEFAULT_DETACHED_CALLS: usize = 16;
pub const MAX_DETACHED_CALLS: usize = 16;
pub const DEFAULT_DETACHED_LOG_BYTES: u64 = 64 * 1024 * 1024;
const MIN_DETACHED_LOG_BYTES: u64 = 1024 * 1024;
const MAX_DETACHED_LOG_BYTES: u64 = 4 * 1024 * 1024 * 1024;
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

pub fn parse_detached_log_bytes(value: Option<&OsStr>) -> io::Result<u64> {
    match value {
        None => Ok(DEFAULT_DETACHED_LOG_BYTES),
        Some(value) => value
            .to_str()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| (MIN_DETACHED_LOG_BYTES..=MAX_DETACHED_LOG_BYTES).contains(value))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{DETACHED_LOG_BYTES_ENV} must be an integer from \
                         {MIN_DETACHED_LOG_BYTES} to {MAX_DETACHED_LOG_BYTES}"
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
    Terminating(StopCause),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopCause {
    Explicit,
    Timeout,
    LogQuota,
    LogQuotaMonitor,
    Shutdown,
}

impl StopCause {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Timeout => "timeout",
            Self::LogQuota => "log_quota",
            Self::LogQuotaMonitor => "log_quota_monitor",
            Self::Shutdown => "shutdown",
        }
    }
}

struct ActiveJob {
    job_id: String,
    pid: u32,
    started_at: Instant,
    timeout: Duration,
    deadline: Instant,
    finished: CancellationToken,
    log_key: String,
    log_path: PathBuf,
    log_reader: Arc<File>,
    phase: ActivePhase,
    primary_exit: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalState {
    Completed,
    Terminated,
    TimedOut,
    OutcomeUncertain,
    LogQuotaExceeded,
}

impl TerminalState {
    const fn model_state(self) -> JobState {
        match self {
            Self::Completed => JobState::Completed,
            Self::Terminated => JobState::Terminated,
            Self::TimedOut => JobState::TimedOut,
            Self::OutcomeUncertain => JobState::OutcomeUncertain,
            Self::LogQuotaExceeded => JobState::LogQuotaExceeded,
        }
    }
}

struct TerminalJob {
    job_id: String,
    pid: u32,
    started_at: Instant,
    finished_at: Instant,
    timeout: Duration,
    log_path: PathBuf,
    state: TerminalState,
    cause: Option<StopCause>,
    primary_exit: Option<String>,
    final_log: RawLogSnapshot,
    log_reader: Arc<File>,
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
    log_quota_bytes: u64,
    state: Arc<Mutex<Roster>>,
    changed: Arc<Condvar>,
    #[cfg(any(test, feature = "test-hooks"))]
    hooks: SharedHooks,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefreshOutcome {
    Stable,
    StatusUnknown,
}

impl DetachedTrees {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self::with_log_quota(capacity, DEFAULT_DETACHED_LOG_BYTES)
    }

    #[must_use]
    pub fn with_log_quota(capacity: usize, log_quota_bytes: u64) -> Self {
        Self {
            capacity: capacity.clamp(1, MAX_DETACHED_CALLS),
            log_quota_bytes: log_quota_bytes.clamp(MIN_DETACHED_LOG_BYTES, MAX_DETACHED_LOG_BYTES),
            state: Arc::new(Mutex::new(Roster::default())),
            changed: Arc::new(Condvar::new()),
            #[cfg(any(test, feature = "test-hooks"))]
            hooks: hooks::shared_default(),
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
        Ok(DetachedAdmission::new(self.clone(), job_id))
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
        timeout: Duration,
        started_at: Instant,
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
            started_at,
            timeout,
            deadline: started_at + timeout,
            finished: CancellationToken::new(),
            log_key: reserved_key.unwrap_or_default(),
            log_path,
            log_reader,
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

    /// Locate one job and render its state-independent snapshot base plus the log
    /// reader and finalized flag the two read paths share.
    fn lookup_job(
        &self,
        job_id: &str,
        refresh: RefreshOutcome,
    ) -> Result<(JobSnapshot, Arc<File>, bool), ProcessError> {
        let state = self.lock();
        if let Some(job) = state.active.iter().find(|job| job.job_id == job_id) {
            let model_state = match (&job.phase, refresh) {
                (ActivePhase::Running(_), RefreshOutcome::StatusUnknown) => JobState::StatusUnknown,
                (ActivePhase::Running(_), RefreshOutcome::Stable) => JobState::Running,
                (ActivePhase::Finalizing, _) => JobState::Finalizing,
                (ActivePhase::Terminating(_), _) => JobState::Terminating,
            };
            Ok((
                active_snapshot(job, model_state, None),
                Arc::clone(&job.log_reader),
                false,
            ))
        } else if let Some(job) = state.terminal.iter().find(|job| job.job_id == job_id) {
            Ok((
                terminal_snapshot(job, None),
                Arc::clone(&job.log_reader),
                true,
            ))
        } else {
            Err(unknown_job_error())
        }
    }

    pub fn status(&self, job_id: &str, tail_bytes: usize) -> Result<JobSnapshot, ProcessError> {
        validate_job_id(job_id)?;
        let refresh = self.refresh_job(job_id);
        let (mut snapshot, reader, finalized) = self.lookup_job(job_id, refresh)?;
        if finalized {
            if tail_bytes < snapshot.log.bytes.len() {
                snapshot.log.bytes = snapshot.log.bytes
                    [snapshot.log.bytes.len().saturating_sub(tail_bytes)..]
                    .to_vec();
            }
        } else {
            snapshot.log = self.snapshot_log(&reader, tail_bytes.min(MAX_TAIL_BYTES));
        }
        Ok(snapshot)
    }

    pub fn status_cursor(
        &self,
        job_id: &str,
        cursor: u64,
        max_bytes: usize,
    ) -> Result<JobSnapshot, ProcessError> {
        validate_job_id(job_id)?;
        let refresh = self.refresh_job(job_id);
        let (mut snapshot, reader, finalized) = self.lookup_job(job_id, refresh)?;
        snapshot.log = crate::tools::bash::status::snapshot_from(
            &reader,
            cursor,
            max_bytes.min(MAX_TAIL_BYTES),
            finalized,
        );
        Ok(snapshot)
    }

    pub fn begin_stop(&self, job_id: &str, cause: StopCause) -> Result<StopStart, ProcessError> {
        validate_job_id(job_id)?;
        let _ = self.refresh_job(job_id);
        let mut state = self.lock();
        if let Some(job) = state.terminal.iter().find(|job| job.job_id == job_id) {
            return Ok(StopStart::Immediate(terminal_snapshot(
                job,
                Some("already_terminal"),
            )));
        }
        let Some(index) = state.active.iter().position(|job| job.job_id == job_id) else {
            return Err(unknown_job_error());
        };
        let job = &mut state.active[index];
        match &job.phase {
            ActivePhase::Finalizing => Ok(StopStart::Immediate(active_snapshot(
                job,
                JobState::Finalizing,
                Some("already_completed"),
            ))),
            ActivePhase::Terminating(_) => Ok(StopStart::Immediate(active_snapshot(
                job,
                JobState::Terminating,
                Some("already_requested"),
            ))),
            ActivePhase::Running(_) => {
                let work = self.take_running_tree(job, cause);
                tracing::info!(target: "agentshim", event = "detached_terminate_accepted", phase = "lifecycle", outcome = "accepted", pid = job.pid);
                self.changed.notify_all();
                Ok(StopStart::Accepted(work))
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
                work.push(self.take_running_tree(job, StopCause::Shutdown));
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

    /// Move one running job into the Terminating phase and claim its process tree
    /// for a terminator; the caller keeps the roster lock and notifies the change.
    fn take_running_tree(&self, job: &mut ActiveJob, cause: StopCause) -> TerminationWork {
        let phase = std::mem::replace(&mut job.phase, ActivePhase::Terminating(cause));
        let ActivePhase::Running(tree) = phase else {
            unreachable!("matched running phase")
        };
        TerminationWork {
            trees: self.clone(),
            job_id: job.job_id.clone(),
            pid: job.pid,
            log_reader: Arc::clone(&job.log_reader),
            primary_exit: job.primary_exit.clone(),
            tree: Some(tree),
            cause,
            completed: false,
        }
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
            if observation.tree_running {
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
            })
        };
        if let Some(work) = finalize {
            let log = self.snapshot_log(&work.log_reader, MAX_TAIL_BYTES);
            self.finish_terminal(&work.job_id, TerminalState::Completed, log, None, None);
        }
        RefreshOutcome::Stable
    }

    fn finish_terminal(
        &self,
        job_id: &str,
        terminal_state: TerminalState,
        final_log: RawLogSnapshot,
        primary_exit: Option<String>,
        cause: Option<StopCause>,
    ) -> JobSnapshot {
        let mut state = self.lock();
        let index = state
            .active
            .iter()
            .position(|job| job.job_id == job_id)
            .expect("terminal transition keeps its active placeholder");
        let mut active = state.active.remove(index);
        active.finished.cancel();
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
            timeout: active.timeout,
            log_path: active.log_path,
            state: terminal_state,
            cause,
            primary_exit: active.primary_exit,
            final_log,
            log_reader: active.log_reader,
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
            state.active.iter().filter_map(|job| {
                matches!(job.phase, ActivePhase::Terminating(_)).then_some(job.pid)
            }),
        );
        pids.sort_unstable();
        pids.dedup();
        pids
    }

    pub fn deadline_registration(&self, job_id: &str) -> Option<DeadlineRegistration> {
        self.lock()
            .active
            .iter()
            .find(|job| job.job_id == job_id)
            .map(|job| DeadlineRegistration {
                job_id: job.job_id.clone(),
                deadline: job.deadline,
                finished: job.finished.clone(),
                log_reader: Arc::clone(&job.log_reader),
                log_quota_bytes: self.log_quota_bytes,
            })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Roster> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn observation(
        &self,
        tree: &mut DetachedTree,
    ) -> io::Result<crate::platform::process::DetachedObservation> {
        #[cfg(any(test, feature = "test-hooks"))]
        if std::mem::take(&mut hooks::lock(&self.hooks).fail_liveness_query) {
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
        if std::mem::take(&mut hooks::lock(&self.hooks).fail_tail_snapshot) {
            return RawLogSnapshot::empty_with_error("injected detached log read failure");
        }
        snapshot_tail(file, tail_bytes)
    }
}

struct FinalizeWork {
    job_id: String,
    log_reader: Arc<File>,
}

pub enum StopStart {
    Immediate(JobSnapshot),
    Accepted(TerminationWork),
}

pub struct TerminationWork {
    trees: DetachedTrees,
    job_id: String,
    pid: u32,
    log_reader: Arc<File>,
    primary_exit: Option<String>,
    tree: Option<DetachedTree>,
    cause: StopCause,
    completed: bool,
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
            match self
                .tree
                .as_mut()
                .expect("stop owner retains the tree")
                .terminate_and_wait(deadline)
            {
                Ok(()) if self.cause == StopCause::Timeout => TerminalState::TimedOut,
                Ok(()) if self.cause == StopCause::LogQuota => TerminalState::LogQuotaExceeded,
                Ok(()) => TerminalState::Terminated,
                Err(_) => TerminalState::OutcomeUncertain,
            }
        };
        if let Ok(observation) = self
            .tree
            .as_mut()
            .expect("stop owner retains the tree")
            .observe()
            && observation.primary_exit.is_some()
        {
            self.primary_exit = observation.primary_exit;
        }
        let final_log = self.trees.snapshot_log(&self.log_reader, MAX_TAIL_BYTES);
        let state = terminal_state.model_state().label();
        let snapshot = self.trees.finish_terminal(
            &self.job_id,
            terminal_state,
            final_log,
            self.primary_exit.clone(),
            Some(self.cause),
        );
        self.completed = true;
        tracing::info!(target: "agentshim", event = "detached_terminate_finished", phase = "lifecycle", outcome = state, pid = self.pid);
        snapshot
    }
}

impl Drop for TerminationWork {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let final_log = self.trees.snapshot_log(&self.log_reader, MAX_TAIL_BYTES);
        self.trees.finish_terminal(
            &self.job_id,
            TerminalState::OutcomeUncertain,
            final_log,
            self.primary_exit.clone(),
            Some(self.cause),
        );
    }
}

#[derive(Clone)]
pub struct DeadlineRegistration {
    job_id: String,
    deadline: Instant,
    finished: CancellationToken,
    log_reader: Arc<File>,
    log_quota_bytes: u64,
}

impl DeadlineRegistration {
    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn finished(&self) -> CancellationToken {
        self.finished.clone()
    }

    pub fn log_quota_exceeded(&self) -> io::Result<bool> {
        self.log_reader
            .metadata()
            .map(|metadata| metadata.len() > self.log_quota_bytes)
    }
}

fn active_snapshot(job: &ActiveJob, state: JobState, outcome: Option<&'static str>) -> JobSnapshot {
    JobSnapshot {
        job_id: job.job_id.clone(),
        state,
        pid: job.pid,
        runtime: job.started_at.elapsed(),
        timeout: job.timeout,
        remaining: Some(job.deadline.saturating_duration_since(Instant::now())),
        cause: match &job.phase {
            ActivePhase::Terminating(cause) => Some(cause.label()),
            ActivePhase::Running(_) | ActivePhase::Finalizing => None,
        },
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
        timeout: job.timeout,
        remaining: Some(Duration::ZERO),
        cause: job.cause.map(StopCause::label),
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
