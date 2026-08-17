use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;

use crate::tools::exec::{
    ProcessError,
    capture::Capture,
    resolve::{ResolvedProgram, launcher_for},
    spawn::{self, EnvironmentPlan, ExecFailure, ExecPlan, Streams},
};

pub const BASH_OVERRIDE_ENV: &str = "AGENTSHIM_BASH";
const FALLBACK_LOCALE: &str = "en_US.UTF-8";
const PREFERRED_LOCALE: &str = "C.UTF-8";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_BUDGET: Duration = Duration::from_secs(15);
const PROBE_OUTPUT_BYTES: usize = 64 * 1024;
const WAIT_SLICE: Duration = Duration::from_millis(10);
const PROBE_MARKER: &str = "AGENTSHIM_BASH_PROBE_V1:";
const PROBE_SCRIPT: &str = "printf 'AGENTSHIM_BASH_PROBE_V1:%s\\n' \"$BASH_VERSION\"\ncommand locale -a 2>/dev/null || true";

#[derive(Clone, Debug)]
pub struct BashRuntime {
    pub executable: PathBuf,
    pub locale: String,
    /// `PATH` the shell runs with, or `None` to inherit the server's unchanged. See
    /// [`toolchain_path`].
    pub path: Option<String>,
}

#[derive(Clone)]
pub struct BashLocator {
    inner: Arc<LocatorInner>,
}

struct LocatorInner {
    inputs: BashInputs,
    state: Mutex<LocatorState>,
    changed: Condvar,
}

enum LocatorState {
    Empty,
    Probing,
    Ready(Result<Arc<BashRuntime>, Arc<str>>),
}

#[derive(Clone)]
struct BashInputs {
    override_path: Option<OsString>,
    candidates: Vec<PathBuf>,
    inherited_path: OsString,
    #[cfg(test)]
    probe_gate: Option<Arc<TestProbeGate>>,
    #[cfg(test)]
    probe_calls: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

#[derive(Debug)]
pub enum LocateError {
    Cancelled,
    TimedOut,
    Unavailable(Arc<str>),
}

impl BashLocator {
    #[must_use]
    pub fn capture() -> Self {
        Self::capture_with_override(None)
    }

    /// Like [`capture`], but an explicit caller-supplied override wins over the
    /// ambient `AGENTSHIM_BASH`. Passing `None` falls back to the ambient value,
    /// keeping [`capture`] behavior identical; a caller that already merged its
    /// own environment (e.g. the DSH napi Engine, which receives a scrubbed
    /// child env separately from the host process env) passes its resolved
    /// override here so plugin config is honored even when the host process
    /// never saw it.
    #[must_use]
    pub fn capture_with_override(override_path: Option<OsString>) -> Self {
        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let override_path = override_path.or_else(|| std::env::var_os(BASH_OVERRIDE_ENV));
        Self::from_inputs(BashInputs {
            override_path,
            candidates: candidates(&inherited_path),
            inherited_path,
            #[cfg(test)]
            probe_gate: None,
            #[cfg(test)]
            probe_calls: None,
        })
    }

    fn from_inputs(inputs: BashInputs) -> Self {
        Self {
            inner: Arc::new(LocatorInner {
                inputs,
                state: Mutex::new(LocatorState::Empty),
                changed: Condvar::new(),
            }),
        }
    }

    /// Probe once for this server instance. Clones share the result, while another
    /// [`BashLocator`] captures and probes its own environment.
    pub fn resolve(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Arc<BashRuntime>, LocateError> {
        self.resolve_with_deadline(cancellation, None)
    }

    pub fn resolve_before(
        &self,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<Arc<BashRuntime>, LocateError> {
        self.resolve_with_deadline(cancellation, Some(deadline))
    }

    fn resolve_with_deadline(
        &self,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Arc<BashRuntime>, LocateError> {
        loop {
            if cancellation.is_cancelled() {
                return Err(LocateError::Cancelled);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(LocateError::TimedOut);
            }
            let mut state = self.lock();
            match &*state {
                LocatorState::Ready(result) => {
                    return result.clone().map_err(LocateError::Unavailable);
                }
                LocatorState::Empty => {
                    *state = LocatorState::Probing;
                    drop(state);
                    return self.probe_as_owner(cancellation, deadline);
                }
                LocatorState::Probing => {
                    let wait = deadline
                        .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
                        .map_or(WAIT_SLICE, |remaining| remaining.min(WAIT_SLICE));
                    if wait.is_zero() {
                        return Err(LocateError::TimedOut);
                    }
                    let waited = self
                        .inner
                        .changed
                        .wait_timeout(state, wait)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    drop(waited.0);
                }
            }
        }
    }

    fn probe_as_owner(
        &self,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Arc<BashRuntime>, LocateError> {
        let mut reset = ProbeReset::new(&self.inner);
        let result = if cancellation.is_cancelled() {
            Err(ProbeError::Cancelled)
        } else {
            let result = probe(&self.inner.inputs, cancellation, deadline);
            if cancellation.is_cancelled() {
                Err(ProbeError::Cancelled)
            } else if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                Err(ProbeError::TimedOut)
            } else {
                result
            }
        };
        let mut state = self.lock();
        match result {
            Ok(runtime) => {
                let runtime = Arc::new(runtime);
                *state = LocatorState::Ready(Ok(Arc::clone(&runtime)));
                reset.disarm();
                self.inner.changed.notify_all();
                Ok(runtime)
            }
            Err(ProbeError::Unavailable(message)) => {
                let message: Arc<str> = message.into();
                *state = LocatorState::Ready(Err(Arc::clone(&message)));
                reset.disarm();
                self.inner.changed.notify_all();
                Err(LocateError::Unavailable(message))
            }
            Err(ProbeError::Cancelled) => {
                *state = LocatorState::Empty;
                reset.disarm();
                self.inner.changed.notify_all();
                Err(LocateError::Cancelled)
            }
            Err(ProbeError::TimedOut) => {
                *state = LocatorState::Empty;
                reset.disarm();
                self.inner.changed.notify_all();
                Err(LocateError::TimedOut)
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LocatorState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub fn for_tests(
        override_path: Option<OsString>,
        candidates: Vec<PathBuf>,
        inherited_path: OsString,
    ) -> Self {
        Self::from_inputs(BashInputs {
            override_path,
            candidates,
            inherited_path,
            probe_gate: None,
            probe_calls: None,
        })
    }

    #[cfg(test)]
    fn with_probe_gate(mut self, gate: Arc<TestProbeGate>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("test locator has not been cloned")
            .inputs
            .probe_gate = Some(gate);
        self
    }

    #[cfg(test)]
    fn with_probe_counter(mut self, calls: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("test locator has not been cloned")
            .inputs
            .probe_calls = Some(calls);
        self
    }
}

struct ProbeReset<'a> {
    inner: &'a LocatorInner,
    armed: bool,
}

impl<'a> ProbeReset<'a> {
    fn new(inner: &'a LocatorInner) -> Self {
        Self { inner, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProbeReset<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = LocatorState::Empty;
        self.inner.changed.notify_all();
    }
}

#[derive(Debug)]
enum ProbeError {
    Cancelled,
    TimedOut,
    Unavailable(String),
}

fn probe(
    inputs: &BashInputs,
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<BashRuntime, ProbeError> {
    let budget = Budget::for_request(deadline);
    #[cfg(test)]
    if let Some(gate) = &inputs.probe_gate {
        gate.wait(cancellation, &budget)?;
        return Ok((*gate.runtime).clone());
    }
    locate(inputs, &budget, cancellation)
}

fn locate(
    inputs: &BashInputs,
    budget: &Budget,
    cancellation: &CancellationToken,
) -> Result<BashRuntime, ProbeError> {
    if let Some(override_path) = &inputs.override_path {
        return validate_override(Path::new(override_path), inputs, budget, cancellation);
    }
    for candidate in &inputs.candidates {
        if cancellation.is_cancelled() {
            return Err(ProbeError::Cancelled);
        }
        if is_excluded(candidate) {
            continue;
        }
        if let Ok(canonical) = std::fs::canonicalize(candidate)
            && let Some(runtime) = probe_candidate(canonical, inputs, budget, cancellation)?
        {
            return Ok(runtime);
        }
    }
    Err(ProbeError::Unavailable(missing_bash_message()))
}

#[cfg(test)]
fn record_probe_for_tests(inputs: &BashInputs) {
    if let Some(calls) = &inputs.probe_calls {
        calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(not(test))]
fn record_probe_for_tests(_inputs: &BashInputs) {}

fn validate_override(
    path: &Path,
    inputs: &BashInputs,
    budget: &Budget,
    cancellation: &CancellationToken,
) -> Result<BashRuntime, ProbeError> {
    if !path.is_absolute() {
        return Err(ProbeError::Unavailable(format!(
            "{BASH_OVERRIDE_ENV} must be an absolute path to a bash executable, got {}",
            path.display()
        )));
    }
    // The raw form first, so a WSL launcher or Store alias is rejected without executing
    // anything; the canonical form also catches a link that resolves into an excluded tree.
    if is_excluded(path) {
        return Err(ProbeError::Unavailable(excluded_override_message(path)));
    }
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        ProbeError::Unavailable(format!("{BASH_OVERRIDE_ENV} cannot be opened: {error}"))
    })?;
    if is_excluded(&canonical) {
        return Err(ProbeError::Unavailable(excluded_override_message(
            &canonical,
        )));
    }
    let Some(runtime) = probe_candidate(canonical.clone(), inputs, budget, cancellation)? else {
        return Err(ProbeError::Unavailable(format!(
            "{BASH_OVERRIDE_ENV} points at {}, which did not complete the Bash probe",
            canonical.display()
        )));
    };
    Ok(runtime)
}

fn excluded_override_message(path: &Path) -> String {
    format!(
        "{BASH_OVERRIDE_ENV} points at {}, which is the WSL launcher or a Store alias and \
         is never used",
        path.display()
    )
}

#[cfg(windows)]
fn candidates(inherited_path: &OsStr) -> Vec<PathBuf> {
    const RELATIVE: &str = "usr/bin/bash.exe";
    let mut candidates = Vec::new();
    if let Some(git) = search_path("git.exe", inherited_path).into_iter().next() {
        let mut directory = git.parent();
        for _ in 0..4 {
            let Some(current) = directory else { break };
            candidates.push(current.join(RELATIVE));
            directory = current.parent();
        }
    }
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(root) = std::env::var_os(variable) {
            candidates.push(Path::new(&root).join("Git").join(RELATIVE));
        }
    }
    if let Some(local) = std::env::var_os("LocalAppData") {
        candidates.push(
            Path::new(&local)
                .join("Programs")
                .join("Git")
                .join(RELATIVE),
        );
    }
    candidates.extend(search_path("bash.exe", inherited_path));
    candidates
}

#[cfg(not(windows))]
fn candidates(inherited_path: &OsStr) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from("/bin/bash"), PathBuf::from("/usr/bin/bash")];
    candidates.extend(search_path("bash", inherited_path));
    candidates
}

/// Every `PATH` hit in order: an earlier hit that discovery excludes must not hide a
/// later Git Bash.
fn search_path(name: &str, inherited_path: &OsStr) -> Vec<PathBuf> {
    std::env::split_paths(inherited_path)
        .filter(|entry| !entry.as_os_str().is_empty())
        .map(|entry| entry.join(name))
        .filter(|candidate| candidate.is_file())
        .collect()
}

#[cfg(windows)]
fn toolchain_path(executable: &Path, inherited_path: &OsStr) -> Option<String> {
    let own = executable.parent()?;
    if !own
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
    {
        return None;
    }
    let mut prefix = Vec::new();
    if let Some(root) = own.parent().and_then(Path::parent) {
        for relative in [
            "usr/local/bin",
            "mingw64/bin",
            "mingw32/bin",
            "clangarm64/bin",
        ] {
            let directory = root.join(relative);
            if directory.is_dir() {
                prefix.push(directory);
            }
        }
    }
    prefix.push(own.to_owned());
    join_ahead_of_inherited(prefix, inherited_path)
}

#[cfg(not(windows))]
fn toolchain_path(_executable: &Path, _inherited_path: &OsStr) -> Option<String> {
    None
}

#[cfg(windows)]
fn join_ahead_of_inherited(prefix: Vec<PathBuf>, inherited_path: &OsStr) -> Option<String> {
    let key = |path: &Path| path.to_string_lossy().to_ascii_lowercase();
    let mut seen = prefix.iter().map(|path| key(path)).collect::<Vec<_>>();
    let mut entries = prefix;
    for entry in std::env::split_paths(inherited_path) {
        if entry.as_os_str().is_empty() {
            continue;
        }
        let entry_key = key(&entry);
        if seen.contains(&entry_key) {
            continue;
        }
        seen.push(entry_key);
        entries.push(entry);
    }
    std::env::join_paths(entries)
        .ok()
        .and_then(|joined| joined.into_string().ok())
}

#[cfg(windows)]
fn is_excluded(candidate: &Path) -> bool {
    let system_root = std::env::var_os("SystemRoot");
    is_excluded_with(candidate, system_root.as_deref())
}

/// Slash forms must agree (`/` and `\` are the same path to Win32), and `SystemRoot` must
/// match on a component boundary so sibling directories like `C:\Windows-Tools` survive.
#[cfg(windows)]
fn is_excluded_with(candidate: &Path, system_root: Option<&OsStr>) -> bool {
    let rendered = normalized_windows_path(candidate);
    if rendered
        .split('\\')
        .any(|component| component == "windowsapps")
    {
        return true;
    }
    let Some(root) = system_root else {
        return false;
    };
    let root = normalized_windows_path(Path::new(root));
    !root.is_empty() && (rendered == root || rendered.starts_with(&format!("{root}\\")))
}

#[cfg(windows)]
fn normalized_windows_path(path: &Path) -> String {
    path.to_string_lossy()
        .to_ascii_lowercase()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_owned()
}

#[cfg(not(windows))]
fn is_excluded(_candidate: &Path) -> bool {
    false
}

fn probe_candidate(
    executable: PathBuf,
    inputs: &BashInputs,
    budget: &Budget,
    cancellation: &CancellationToken,
) -> Result<Option<BashRuntime>, ProbeError> {
    record_probe_for_tests(inputs);
    let path = toolchain_path(&executable, &inputs.inherited_path);
    let output = probe_output(
        &executable,
        &["--noprofile", "--norc", "-c", PROBE_SCRIPT],
        path.as_deref(),
        budget,
        cancellation,
    )?;
    Ok(output
        .as_deref()
        .and_then(parse_probe_output)
        .map(|locale| BashRuntime {
            executable,
            locale,
            path,
        }))
}

fn parse_probe_output(output: &str) -> Option<String> {
    let mut lines = output.lines();
    let version = lines.next()?.strip_prefix(PROBE_MARKER)?.trim();
    if version.is_empty() {
        return None;
    }
    let preferred = lines
        .any(|line| line.trim().eq_ignore_ascii_case(PREFERRED_LOCALE) || line.trim() == "C.utf8");
    Some(if preferred {
        PREFERRED_LOCALE.to_owned()
    } else {
        FALLBACK_LOCALE.to_owned()
    })
}

struct Budget {
    deadline: Instant,
    request_deadline: Option<Instant>,
}

impl Budget {
    #[cfg(test)]
    fn new(total: Duration) -> Self {
        Self {
            deadline: Instant::now() + total,
            request_deadline: None,
        }
    }

    fn for_request(request_deadline: Option<Instant>) -> Self {
        let probe_deadline = Instant::now() + PROBE_BUDGET;
        Self {
            deadline: request_deadline
                .map_or(probe_deadline, |deadline| deadline.min(probe_deadline)),
            request_deadline,
        }
    }

    fn slice(&self) -> Option<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| remaining.min(PROBE_TIMEOUT))
    }

    fn request_expired(&self) -> bool {
        self.request_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }
}

fn probe_output(
    program: &Path,
    args: &[&str],
    path: Option<&str>,
    budget: &Budget,
    cancellation: &CancellationToken,
) -> Result<Option<String>, ProbeError> {
    let Some(cwd) = program.parent() else {
        return Ok(None);
    };
    probe_output_in(program, args, path, budget, cancellation, cwd)
}

fn probe_output_in(
    program: &Path,
    args: &[&str],
    path: Option<&str>,
    budget: &Budget,
    cancellation: &CancellationToken,
    cwd: &Path,
) -> Result<Option<String>, ProbeError> {
    let Some(timeout) = budget.slice() else {
        return if budget.request_expired() {
            Err(ProbeError::TimedOut)
        } else {
            Ok(None)
        };
    };
    if cancellation.is_cancelled() {
        return Err(ProbeError::Cancelled);
    }
    let resolved = ResolvedProgram {
        absolute: program.to_owned(),
        executable: program.to_owned(),
        launcher: match launcher_for(program) {
            Ok(launcher) => launcher,
            Err(_) => return Ok(None),
        },
    };
    let mut environment = EnvironmentPlan::default();
    environment
        .removed
        .extend(super::STRIPPED_INHERITED_ENV.map(str::to_owned));
    if let Some(path) = path {
        environment
            .overrides
            .push(("PATH".to_owned(), path.to_owned()));
    }
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    let plan = ExecPlan {
        resolved: &resolved,
        cwd,
        args: &args,
        environment: &environment,
        stdin: None,
        streams: Streams::Merged,
        timeout,
        capture_page_bytes: crate::output::MODEL_BYTE_LIMIT,
    };
    match spawn::run(&plan, cancellation) {
        Ok(outcome) if outcome.exit == "0" => Ok(probe_capture(outcome.captures)),
        Err(ExecFailure::Process(ProcessError::Cancelled)) => Err(ProbeError::Cancelled),
        Err(ExecFailure::TimedOut { .. }) if budget.request_expired() => Err(ProbeError::TimedOut),
        Ok(_) | Err(ExecFailure::TimedOut { .. } | ExecFailure::Process(_)) => Ok(None),
    }
}

fn probe_capture(mut captures: Vec<Capture>) -> Option<String> {
    let capture = captures.pop()?;
    if !captures.is_empty() || capture.bytes_read > PROBE_OUTPUT_BYTES {
        return None;
    }
    Some(capture.render(PROBE_OUTPUT_BYTES).text)
}

#[cfg(windows)]
fn missing_bash_message() -> String {
    format!(
        "no GNU bash was found. Install Git for Windows (https://git-scm.com/download/win), \
         which provides <install>\\usr\\bin\\bash.exe, or set {BASH_OVERRIDE_ENV} to the \
         absolute path of a bash.exe. C:\\Windows\\System32\\bash.exe is the WSL launcher, \
         not a standalone bash, and is never used"
    )
}

#[cfg(not(windows))]
fn missing_bash_message() -> String {
    format!(
        "no GNU bash was found at /bin/bash, /usr/bin/bash, or on PATH. Install bash or set \
         {BASH_OVERRIDE_ENV} to the absolute path of a bash executable"
    )
}

#[cfg(test)]
struct TestProbeGate {
    entered: std::sync::Barrier,
    released: (Mutex<bool>, Condvar),
    calls: std::sync::atomic::AtomicUsize,
    runtime: Arc<BashRuntime>,
}

#[cfg(test)]
impl TestProbeGate {
    fn new(runtime: Arc<BashRuntime>) -> Self {
        Self {
            entered: std::sync::Barrier::new(2),
            released: (Mutex::new(false), Condvar::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            runtime,
        }
    }

    fn wait(&self, cancellation: &CancellationToken, budget: &Budget) -> Result<(), ProbeError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) != 0 {
            return Ok(());
        }
        self.entered.wait();
        let (lock, changed) = &self.released;
        let mut released = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if cancellation.is_cancelled() {
                return Err(ProbeError::Cancelled);
            }
            if budget.request_expired() {
                return Err(ProbeError::TimedOut);
            }
            if *released {
                return Ok(());
            }
            let wait = budget
                .deadline
                .checked_duration_since(Instant::now())
                .map_or(Duration::ZERO, |remaining| remaining.min(WAIT_SLICE));
            if wait.is_zero() {
                return if budget.request_expired() {
                    Err(ProbeError::TimedOut)
                } else {
                    Ok(())
                };
            }
            released = changed
                .wait_timeout(released, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    fn release(&self) {
        let (lock, changed) = &self.released;
        *lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        changed.notify_all();
    }
}

#[cfg(test)]
#[path = "locate/tests.rs"]
mod tests;
