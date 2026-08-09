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

pub(crate) const BASH_OVERRIDE_ENV: &str = "CODEXSHIM_BASH";
const FALLBACK_LOCALE: &str = "en_US.UTF-8";
const PREFERRED_LOCALE: &str = "C.UTF-8";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_BUDGET: Duration = Duration::from_secs(15);
const PROBE_OUTPUT_BYTES: usize = 64 * 1024;
const WAIT_SLICE: Duration = Duration::from_millis(10);

#[derive(Clone, Debug)]
pub(crate) struct BashRuntime {
    pub(crate) executable: PathBuf,
    pub(crate) locale: String,
    /// `PATH` the shell runs with, or `None` to inherit the server's unchanged. See
    /// [`toolchain_path`].
    pub(crate) path: Option<String>,
}

#[derive(Clone)]
pub(crate) struct BashLocator {
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
}

#[derive(Debug)]
pub(crate) enum LocateError {
    Cancelled,
    Unavailable(Arc<str>),
}

impl BashLocator {
    #[must_use]
    pub(crate) fn capture() -> Self {
        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        Self::from_inputs(BashInputs {
            override_path: std::env::var_os(BASH_OVERRIDE_ENV),
            candidates: candidates(&inherited_path),
            inherited_path,
            #[cfg(test)]
            probe_gate: None,
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
    pub(crate) fn resolve(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Arc<BashRuntime>, LocateError> {
        loop {
            if cancellation.is_cancelled() {
                return Err(LocateError::Cancelled);
            }
            let mut state = self.lock();
            match &*state {
                LocatorState::Ready(result) => {
                    return result.clone().map_err(LocateError::Unavailable);
                }
                LocatorState::Empty => {
                    *state = LocatorState::Probing;
                    drop(state);
                    return self.probe_as_owner(cancellation);
                }
                LocatorState::Probing => {
                    let waited = self
                        .inner
                        .changed
                        .wait_timeout(state, WAIT_SLICE)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    drop(waited.0);
                }
            }
        }
    }

    fn probe_as_owner(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Arc<BashRuntime>, LocateError> {
        let mut reset = ProbeReset::new(&self.inner);
        let result = if cancellation.is_cancelled() {
            Err(ProbeError::Cancelled)
        } else {
            let result = probe(&self.inner.inputs, cancellation);
            if cancellation.is_cancelled() {
                Err(ProbeError::Cancelled)
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
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LocatorState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn for_tests(
        override_path: Option<OsString>,
        candidates: Vec<PathBuf>,
        inherited_path: OsString,
    ) -> Self {
        Self::from_inputs(BashInputs {
            override_path,
            candidates,
            inherited_path,
            probe_gate: None,
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
    Unavailable(String),
}

fn probe(inputs: &BashInputs, cancellation: &CancellationToken) -> Result<BashRuntime, ProbeError> {
    #[cfg(test)]
    if let Some(gate) = &inputs.probe_gate {
        gate.wait(cancellation)?;
    }
    let budget = Budget::new(PROBE_BUDGET);
    let executable = locate(inputs, &budget, cancellation)?;
    let path = toolchain_path(&executable, &inputs.inherited_path);
    let locale = detect_locale(&executable, path.as_deref(), &budget, cancellation)?;
    Ok(BashRuntime {
        executable,
        locale,
        path,
    })
}

fn locate(
    inputs: &BashInputs,
    budget: &Budget,
    cancellation: &CancellationToken,
) -> Result<PathBuf, ProbeError> {
    if let Some(override_path) = &inputs.override_path {
        return validate_override(Path::new(override_path), budget, cancellation);
    }
    for candidate in &inputs.candidates {
        if cancellation.is_cancelled() {
            return Err(ProbeError::Cancelled);
        }
        if is_excluded(candidate) {
            continue;
        }
        if let Ok(canonical) = std::fs::canonicalize(candidate)
            && reports_gnu_bash(&canonical, budget, cancellation)?
        {
            return Ok(canonical);
        }
    }
    Err(ProbeError::Unavailable(missing_bash_message()))
}

fn validate_override(
    path: &Path,
    budget: &Budget,
    cancellation: &CancellationToken,
) -> Result<PathBuf, ProbeError> {
    if !path.is_absolute() {
        return Err(ProbeError::Unavailable(format!(
            "{BASH_OVERRIDE_ENV} must be an absolute path to a bash executable, got {}",
            path.display()
        )));
    }
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        ProbeError::Unavailable(format!("{BASH_OVERRIDE_ENV} cannot be opened: {error}"))
    })?;
    if !reports_gnu_bash(&canonical, budget, cancellation)? {
        return Err(ProbeError::Unavailable(format!(
            "{BASH_OVERRIDE_ENV} points at {}, which did not report GNU bash from `--version`",
            canonical.display()
        )));
    }
    Ok(canonical)
}

#[cfg(windows)]
fn candidates(inherited_path: &OsStr) -> Vec<PathBuf> {
    const RELATIVE: &str = "usr/bin/bash.exe";
    let mut candidates = Vec::new();
    if let Some(git) = search_path("git.exe", inherited_path) {
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
    if let Some(path_bash) = search_path("bash.exe", inherited_path) {
        candidates.push(path_bash);
    }
    candidates
}

#[cfg(not(windows))]
fn candidates(inherited_path: &OsStr) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from("/bin/bash"), PathBuf::from("/usr/bin/bash")];
    if let Some(path_bash) = search_path("bash", inherited_path) {
        candidates.push(path_bash);
    }
    candidates
}

fn search_path(name: &str, inherited_path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(inherited_path)
        .filter(|entry| !entry.as_os_str().is_empty())
        .map(|entry| entry.join(name))
        .find(|candidate| candidate.is_file())
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
        for relative in ["usr/local/bin", "mingw64/bin", "mingw32/bin"] {
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
    let rendered = candidate.to_string_lossy().to_ascii_lowercase();
    if rendered.contains("\\windowsapps\\") {
        return true;
    }
    std::env::var_os("SystemRoot").is_some_and(|root| {
        let root = root.to_string_lossy().to_ascii_lowercase();
        !root.is_empty() && rendered.starts_with(&root)
    })
}

#[cfg(not(windows))]
fn is_excluded(_candidate: &Path) -> bool {
    false
}

fn reports_gnu_bash(
    candidate: &Path,
    budget: &Budget,
    cancellation: &CancellationToken,
) -> Result<bool, ProbeError> {
    probe_output(candidate, &["--version"], None, budget, cancellation)
        .map(|output| output.is_some_and(|output| output.contains("GNU bash")))
}

fn detect_locale(
    executable: &Path,
    path: Option<&str>,
    budget: &Budget,
    cancellation: &CancellationToken,
) -> Result<String, ProbeError> {
    let available = probe_output(
        executable,
        &["--noprofile", "--norc", "-c", "locale -a"],
        path,
        budget,
        cancellation,
    )?;
    let matched = available.is_some_and(|listing| {
        listing.lines().any(|line| {
            line.trim().eq_ignore_ascii_case(PREFERRED_LOCALE) || line.trim() == "C.utf8"
        })
    });
    Ok(if matched {
        PREFERRED_LOCALE.to_owned()
    } else {
        FALLBACK_LOCALE.to_owned()
    })
}

struct Budget {
    deadline: Instant,
}

impl Budget {
    fn new(total: Duration) -> Self {
        Self {
            deadline: Instant::now() + total,
        }
    }

    fn slice(&self) -> Option<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| remaining.min(PROBE_TIMEOUT))
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
        return Ok(None);
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
    };
    match spawn::run(&plan, cancellation) {
        Ok(outcome) if outcome.exit == "0" => Ok(probe_capture(outcome.captures)),
        Err(ExecFailure::Process(ProcessError::Cancelled)) => Err(ProbeError::Cancelled),
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
}

#[cfg(test)]
impl TestProbeGate {
    fn new() -> Self {
        Self {
            entered: std::sync::Barrier::new(2),
            released: (Mutex::new(false), Condvar::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn wait(&self, cancellation: &CancellationToken) -> Result<(), ProbeError> {
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
            if *released {
                return Ok(());
            }
            released = changed
                .wait_timeout(released, WAIT_SLICE)
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
mod tests {
    use super::*;

    fn available_bash() -> Option<Arc<BashRuntime>> {
        BashLocator::capture()
            .resolve(&CancellationToken::new())
            .ok()
    }

    #[test]
    fn locator_instances_keep_captured_inputs_independent() {
        let Some(runtime) = available_bash() else {
            return;
        };
        let valid = BashLocator::for_tests(
            Some(runtime.executable.clone().into_os_string()),
            Vec::new(),
            std::env::var_os("PATH").unwrap_or_default(),
        );
        let missing_path = std::env::temp_dir().join("codexshim-definitely-missing-bash");
        let invalid = BashLocator::for_tests(
            Some(missing_path.clone().into_os_string()),
            Vec::new(),
            OsString::new(),
        );

        assert!(valid.resolve(&CancellationToken::new()).is_ok());
        let error = invalid
            .resolve(&CancellationToken::new())
            .expect_err("the second locator has different captured inputs");
        assert!(matches!(error, LocateError::Unavailable(_)));
    }

    #[test]
    fn cancelled_probe_returns_to_empty_and_can_be_retried() {
        let Some(runtime) = available_bash() else {
            return;
        };
        let gate = Arc::new(TestProbeGate::new());
        let locator = BashLocator::for_tests(
            Some(runtime.executable.clone().into_os_string()),
            Vec::new(),
            std::env::var_os("PATH").unwrap_or_default(),
        )
        .with_probe_gate(Arc::clone(&gate));
        let cancellation = CancellationToken::new();
        let worker_locator = locator.clone();
        let worker_cancellation = cancellation.clone();
        let worker = std::thread::spawn(move || worker_locator.resolve(&worker_cancellation));
        gate.entered.wait();
        cancellation.cancel();

        assert!(matches!(
            worker.join().expect("probe worker"),
            Err(LocateError::Cancelled)
        ));
        assert!(locator.resolve(&CancellationToken::new()).is_ok());
    }

    #[test]
    fn a_waiter_can_cancel_without_cancelling_the_shared_probe() {
        let Some(runtime) = available_bash() else {
            return;
        };
        let gate = Arc::new(TestProbeGate::new());
        let locator = BashLocator::for_tests(
            Some(runtime.executable.clone().into_os_string()),
            Vec::new(),
            std::env::var_os("PATH").unwrap_or_default(),
        )
        .with_probe_gate(Arc::clone(&gate));
        let owner_locator = locator.clone();
        let owner = std::thread::spawn(move || owner_locator.resolve(&CancellationToken::new()));
        gate.entered.wait();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            locator.resolve(&cancelled),
            Err(LocateError::Cancelled)
        ));
        gate.release();
        assert!(owner.join().expect("owner").is_ok());
        assert!(locator.resolve(&CancellationToken::new()).is_ok());
    }

    #[test]
    fn probe_rejects_output_over_sixty_four_kibibytes() {
        let Some(runtime) = available_bash() else {
            return;
        };
        let budget = Budget::new(Duration::from_secs(5));
        let output = probe_output(
            &runtime.executable,
            &[
                "--noprofile",
                "--norc",
                "-c",
                "head -c 65537 /dev/zero | tr '\\0' x",
            ],
            runtime.path.as_deref(),
            &budget,
            &CancellationToken::new(),
        )
        .expect("probe execution");

        assert!(output.is_none());
    }

    #[test]
    fn probe_timeout_terminates_the_process_tree() {
        let Some(runtime) = available_bash() else {
            return;
        };
        let budget = Budget::new(Duration::from_millis(100));
        let started = Instant::now();
        let output = probe_output(
            &runtime.executable,
            &["--noprofile", "--norc", "-c", "sleep 30"],
            runtime.path.as_deref(),
            &budget,
            &CancellationToken::new(),
        )
        .expect("probe execution");

        assert!(output.is_none());
        assert!(started.elapsed() < Duration::from_secs(6));
    }

    #[test]
    fn successful_primary_with_a_pipe_holding_descendant_is_bounded() {
        let Some(runtime) = available_bash() else {
            return;
        };
        let fixture = tempfile::tempdir().expect("fixture");
        let budget = Budget::new(Duration::from_secs(5));
        let started = Instant::now();
        let output = probe_output_in(
            &runtime.executable,
            &[
                "--noprofile",
                "--norc",
                "-c",
                "(sleep 1; printf late > delayed-marker) & printf 'probe complete\\n'",
            ],
            runtime.path.as_deref(),
            &budget,
            &CancellationToken::new(),
            fixture.path(),
        )
        .expect("probe execution")
        .expect("successful probe output");

        assert!(output.contains("probe complete"));
        assert!(started.elapsed() < Duration::from_secs(6));
        std::thread::sleep(Duration::from_millis(1_250));
        assert!(
            !fixture.path().join("delayed-marker").exists(),
            "probe descendant survived process-tree cleanup"
        );
    }

    #[test]
    fn cancelling_a_running_probe_terminates_its_process_tree() {
        let Some(runtime) = available_bash() else {
            return;
        };
        let fixture = tempfile::tempdir().expect("fixture");
        let cwd = fixture.path().to_owned();
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let worker = std::thread::spawn(move || {
            let budget = Budget::new(Duration::from_secs(5));
            probe_output_in(
                &runtime.executable,
                &[
                    "--noprofile",
                    "--norc",
                    "-c",
                    "(sleep 1; printf late > delayed-marker) & wait",
                ],
                runtime.path.as_deref(),
                &budget,
                &worker_cancellation,
                &cwd,
            )
        });
        std::thread::sleep(Duration::from_millis(100));
        cancellation.cancel();

        assert!(matches!(
            worker.join().expect("probe worker"),
            Err(ProbeError::Cancelled)
        ));
        std::thread::sleep(Duration::from_millis(1_250));
        assert!(
            !fixture.path().join("delayed-marker").exists(),
            "cancelled probe descendant survived process-tree cleanup"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_git_layout_gains_its_own_toolchain_directories_ahead_of_the_inherited_path() {
        let Some(runtime) = available_bash() else {
            return;
        };
        let own = runtime.executable.parent().expect("bash parent directory");
        if !own
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
        {
            return;
        }
        let path = runtime
            .path
            .as_deref()
            .expect("a bin layout must yield a toolchain PATH");
        let first = std::env::split_paths(path).next().expect("a first entry");

        assert!(std::env::split_paths(path).any(|entry| entry == own));
        assert!(first.is_dir());
        for inherited in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            if !inherited.as_os_str().is_empty() {
                assert!(std::env::split_paths(path).any(|entry| entry == inherited));
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn an_unrecognised_layout_leaves_the_inherited_path_alone() {
        assert!(
            toolchain_path(Path::new(r"C:\tools\bash.exe"), OsStr::new("C:\\Windows")).is_none()
        );
    }
}
