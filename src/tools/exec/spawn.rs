use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(windows)]
use std::{
    io,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use tokio_util::sync::CancellationToken;

use crate::path::RepositoryRoot;

use super::{
    ProcessError,
    capture::Capture,
    resolve::{Launcher, ResolvedProgram},
};

pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// The `tool_timeout_sec` ceiling the client examples and READMEs document. The server's own
/// execution ceiling stays below this so a client configured at the shelf always receives the
/// server's Timeout response before its `tool_timeout_sec` fires.
pub(super) const TOOL_TIMEOUT_SHELF: Duration = Duration::from_secs(600);
/// Round-trip slack beyond `CLEANUP_DEADLINE` for the MCP response carrying the Timeout.
pub(super) const PROTOCOL_SLACK: Duration = Duration::from_secs(5);
/// Maximum execution time the caller may request. Derived below `TOOL_TIMEOUT_SHELF` by the
/// cleanup deadline plus protocol slack, so the client never gives up before the server does.
#[allow(
    clippy::cast_possible_truncation,
    reason = "TOOL_TIMEOUT_SHELF is 600 s, far below u64::MAX ms"
)]
pub(crate) const MAX_TIMEOUT_MS: u64 = TOOL_TIMEOUT_SHELF
    .saturating_sub(CLEANUP_DEADLINE)
    .saturating_sub(PROTOCOL_SLACK)
    .as_millis() as u64;
#[cfg(unix)]
pub(crate) const TERM_GRACE: Duration = Duration::from_millis(250);
pub(crate) const CLEANUP_DEADLINE: Duration = Duration::from_secs(5);
#[cfg(windows)]
pub(crate) const IO_CANCELLATION_DEADLINE: Duration = Duration::from_secs(1);
/// How long descendants may outlive the primary process before the owned containment is
/// terminated. A detached tree is how a command intentionally outlives its call.
pub(crate) const DESCENDANT_EXIT_GRACE: Duration = Duration::from_millis(250);

/// Pipe topology for one call. `Merged` points the child's stdout and stderr at a single pipe
/// so the parent observes both in pipe-write order; it cannot attribute a line to a stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Streams {
    Separate,
    Merged,
}

impl Streams {
    pub(crate) fn count(self) -> usize {
        match self {
            Self::Separate => 2,
            Self::Merged => 1,
        }
    }
}

/// Environment mutations applied in order: `injected`, then `removed`, then `overrides`.
/// Facets differ only in what they put here; the core never names a tool.
#[derive(Clone, Debug, Default)]
pub(crate) struct EnvironmentPlan {
    pub injected: Vec<(String, String)>,
    pub removed: Vec<String>,
    pub overrides: Vec<(String, String)>,
}

impl EnvironmentPlan {
    pub(crate) fn from_defaults(defaults: &[(&str, &str)]) -> Self {
        Self {
            injected: defaults
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            removed: Vec::new(),
            overrides: Vec::new(),
        }
    }
}

pub(crate) struct ExecPlan<'a> {
    pub resolved: &'a ResolvedProgram,
    pub cwd: &'a Path,
    pub args: &'a [String],
    pub environment: &'a EnvironmentPlan,
    pub stdin: Option<&'a str>,
    pub streams: Streams,
    pub timeout: Duration,
}

pub(crate) struct ExecOutcome {
    pub exit: String,
    pub duration: Duration,
    pub captures: Vec<Capture>,
}

pub(crate) enum ExecFailure {
    Process(ProcessError),
    TimedOut {
        duration: Duration,
        captures: Vec<Capture>,
    },
}

impl From<ProcessError> for ExecFailure {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

impl From<std::io::Error> for ExecFailure {
    fn from(error: std::io::Error) -> Self {
        Self::Process(ProcessError::Io(error))
    }
}

impl ExecFailure {
    pub(crate) fn into_process_error(self, timeout_ms: u64) -> ProcessError {
        match self {
            Self::Process(error) => error,
            Self::TimedOut { .. } => ProcessError::TimeoutBeforeSpawn { timeout_ms },
        }
    }
}

pub(crate) fn run(
    plan: &ExecPlan<'_>,
    cancellation: &CancellationToken,
) -> Result<ExecOutcome, ExecFailure> {
    tracing::info!(target: "codexshim", event = "process_spawn", phase = "execution");
    let result = crate::platform::process::run(plan, cancellation);
    match &result {
        Ok(_) => tracing::info!(target: "codexshim", event = "process_exit", phase = "execution"),
        Err(ExecFailure::TimedOut { .. }) => {
            tracing::error!(target: "codexshim", event = "process_timeout", phase = "cleanup", error_class = "resource_timeout");
        }
        Err(ExecFailure::Process(ProcessError::OutcomeUncertain)) => {
            tracing::error!(target: "codexshim", event = "process_cleanup", phase = "cleanup", outcome = "uncertain", error_class = "outcome_uncertain");
        }
        Err(_) => {}
    }
    result
}

pub(crate) fn launcher_label(resolved: &ResolvedProgram) -> &'static str {
    Launcher::label(resolved.launcher)
}

/// Resolve a requested working directory. Absolute paths deliberately escape the repository
/// root; relative paths stay inside it through the capability.
pub(crate) fn resolve_cwd(
    root: &RepositoryRoot,
    requested: Option<&str>,
) -> Result<PathBuf, String> {
    let candidate = requested.map_or_else(|| root.path().to_owned(), PathBuf::from);
    let candidate = if candidate.is_absolute() {
        candidate
    } else {
        root.resolve(&candidate)
            .map_err(|error| format!("invalid relative cwd: {error}"))?
            .absolute()
            .to_owned()
    };
    let cwd = fs::canonicalize(&candidate)
        .map_err(|error| format!("cannot resolve cwd {}: {error}", candidate.display()))?;
    if !cwd.is_dir() {
        return Err(format!("cwd is not a directory: {}", cwd.display()));
    }
    Ok(cwd)
}

#[cfg(unix)]
pub(crate) fn apply_environment(command: &mut std::process::Command, plan: &EnvironmentPlan) {
    for (key, value) in &plan.injected {
        command.env(key, value);
    }
    for key in &plan.removed {
        command.env_remove(key);
    }
    for (key, value) in &plan.overrides {
        command.env(key, value);
    }
}

#[cfg(windows)]
pub(crate) fn spawn_monitored<T: Send + 'static>(
    failed: Arc<AtomicBool>,
    completion: ThreadCompletion,
    task: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> std::thread::JoinHandle<io::Result<T>> {
    std::thread::spawn(move || {
        let _completion = completion.signal_on_drop();
        let result = task();
        if result.is_err() {
            failed.store(true, Ordering::Release);
        }
        result
    })
}

#[cfg(windows)]
#[derive(Clone)]
pub(crate) struct ThreadCompletion {
    state: Arc<(Mutex<usize>, Condvar)>,
}

#[cfg(windows)]
impl ThreadCompletion {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new((Mutex::new(0), Condvar::new())),
        }
    }

    fn signal_on_drop(&self) -> CompletionSignal {
        CompletionSignal(self.clone())
    }

    pub(crate) fn wait_for(&self, count: usize, timeout: Duration) -> bool {
        let (lock, changed) = &*self.state;
        let mut completed = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = Instant::now() + timeout;
        while *completed < count {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let result = changed
                .wait_timeout(completed, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            completed = result.0;
            if result.1.timed_out() && *completed < count {
                return false;
            }
        }
        true
    }
}

#[cfg(windows)]
struct CompletionSignal(ThreadCompletion);

#[cfg(windows)]
impl Drop for CompletionSignal {
    fn drop(&mut self) {
        let (lock, changed) = &*self.0.state;
        let mut completed = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *completed = completed.saturating_add(1);
        changed.notify_all();
    }
}
