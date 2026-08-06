use std::{
    collections::{BTreeMap, VecDeque},
    env, fs, io,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    output::{OutputFormatter, OutputLimits},
    path::RepositoryRoot,
};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const MAX_STDIN_BYTES: usize = 1024 * 1024;
const CAPTURE_HEAD_BYTES: usize = 12 * 1024;
const CAPTURE_TAIL_BYTES: usize = 12 * 1024;
const DRAIN_CHUNK_BYTES: usize = 64 * 1024;
const DIAGNOSTIC_PATH_BYTES: usize = 2 * 1024;
const DIAGNOSTIC_PATH_MARKER: &str = "...[path truncated]...";
#[cfg(unix)]
const TERM_GRACE: Duration = Duration::from_millis(250);
const CLEANUP_DEADLINE: Duration = Duration::from_secs(5);

const ENVIRONMENT_DEFAULTS: [(&str, &str); 6] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_PAGER", "cat"),
    ("PAGER", "cat"),
    ("CARGO_TERM_COLOR", "never"),
];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessRequest {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub unset_env: Vec<String>,
    pub stdin: Option<String>,
    pub timeout_ms: Option<u64>,
}

impl ProcessRequest {
    /// Validate all scalar and environment constraints before process admission.
    ///
    /// # Errors
    ///
    /// Returns a validation error for malformed, conflicting, or oversized input.
    pub fn validate(&self) -> Result<(), ProcessError> {
        if self.program.is_empty() {
            return Err(ProcessError::Validation(
                "program must not be empty".to_owned(),
            ));
        }
        if contains_nul(&self.program)
            || self.args.iter().any(|arg| contains_nul(arg))
            || self.cwd.as_deref().is_some_and(contains_nul)
            || self.stdin.as_deref().is_some_and(contains_nul)
        {
            return Err(ProcessError::Validation(
                "program, args, cwd, and stdin must not contain NUL".to_owned(),
            ));
        }
        if self
            .stdin
            .as_ref()
            .is_some_and(|stdin| stdin.len() > MAX_STDIN_BYTES)
        {
            return Err(ProcessError::Validation(
                "stdin must not exceed 1048576 UTF-8 bytes".to_owned(),
            ));
        }
        if !(1..=MAX_TIMEOUT_MS).contains(&self.timeout_ms()) {
            return Err(ProcessError::Validation(
                "timeout_ms must be from 1 to 300000".to_owned(),
            ));
        }

        let mut overrides: Vec<String> = Vec::new();
        for (key, value) in &self.env {
            validate_environment(key, value)?;
            if overrides
                .iter()
                .any(|existing| environment_keys_equal(existing, key))
            {
                return Err(ProcessError::Validation(format!(
                    "env contains duplicate key {key:?} under platform comparison rules"
                )));
            }
            overrides.push(key.clone());
        }
        let mut removals: Vec<String> = Vec::new();
        for key in &self.unset_env {
            validate_environment(key, "")?;
            if removals
                .iter()
                .any(|existing| environment_keys_equal(existing, key))
            {
                return Err(ProcessError::Validation(format!(
                    "unset_env contains duplicate key {key:?}"
                )));
            }
            if overrides
                .iter()
                .any(|existing| environment_keys_equal(existing, key))
            {
                return Err(ProcessError::Validation(format!(
                    "environment key {key:?} occurs in both env and unset_env"
                )));
            }
            removals.push(key.clone());
        }
        Ok(())
    }

    #[must_use]
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)
    }

    #[must_use]
    pub fn memory_charge(&self) -> usize {
        let strings = self
            .args
            .iter()
            .map(String::len)
            .chain(self.env.iter().map(|(key, value)| key.len() + value.len()))
            .chain(self.unset_env.iter().map(String::len))
            .sum::<usize>();
        256_usize
            .saturating_mul(1024)
            .saturating_add(self.program.len())
            .saturating_add(self.cwd.as_deref().map_or(0, str::len))
            .saturating_add(self.stdin.as_deref().map_or(0, str::len))
            .saturating_add(strings)
    }
}

fn contains_nul(value: &str) -> bool {
    value.contains('\0')
}

fn validate_environment(key: &str, value: &str) -> Result<(), ProcessError> {
    if key.is_empty() || key.contains('=') || contains_nul(key) || contains_nul(value) {
        return Err(ProcessError::Validation(format!(
            "invalid environment key or value for {key:?}"
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn environment_keys_equal(left: &str, right: &str) -> bool {
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};

    let left = left.encode_utf16().collect::<Vec<_>>();
    let right = right.encode_utf16().collect::<Vec<_>>();
    let left_length = i32::try_from(left.len()).unwrap_or(i32::MAX);
    let right_length = i32::try_from(right.len()).unwrap_or(i32::MAX);
    unsafe {
        CompareStringOrdinal(left.as_ptr(), left_length, right.as_ptr(), right_length, 1)
            == CSTR_EQUAL
    }
}

#[cfg(not(windows))]
fn environment_keys_equal(left: &str, right: &str) -> bool {
    left == right
}

#[derive(Clone, Debug)]
pub struct ProcessResolver {
    search_path: Arc<[PathBuf]>,
    #[cfg(windows)]
    path_extensions: Arc<[String]>,
}

impl ProcessResolver {
    #[must_use]
    pub fn capture() -> Self {
        let search_path = env::var_os("PATH")
            .map(|path| {
                env::split_paths(&path)
                    .filter(|entry| !entry.as_os_str().is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
            .into();
        #[cfg(windows)]
        let path_extensions = controlled_path_extensions();
        Self {
            search_path,
            #[cfg(windows)]
            path_extensions,
        }
    }

    #[cfg(all(test, unix))]
    fn for_tests(search_path: Vec<PathBuf>) -> Self {
        Self {
            search_path: search_path.into(),
            #[cfg(windows)]
            path_extensions: vec![".exe".to_owned(), ".com".to_owned()].into(),
        }
    }

    fn resolve(&self, program: &str, cwd: &Path) -> Result<ResolvedProgram, ProcessError> {
        let requested = Path::new(program);
        if requested.is_absolute() {
            return resolve_candidate(requested);
        }
        if has_separator(program) {
            return resolve_candidate(&cwd.join(requested));
        }
        for directory in self.search_path.iter() {
            let directory = if directory.is_absolute() {
                directory.clone()
            } else {
                cwd.join(directory)
            };
            #[cfg(not(windows))]
            let candidates = Self::candidates(&directory, program);
            #[cfg(windows)]
            let candidates = self.candidates(&directory, program);
            for candidate in candidates {
                match resolve_candidate(&candidate) {
                    Ok(resolved) => return Ok(resolved),
                    Err(error) if candidate.exists() => return Err(error),
                    Err(_) => {}
                }
            }
        }
        Err(ProcessError::Resolve(format!(
            "program {program:?} was not found in the captured PATH"
        )))
    }

    #[cfg(not(windows))]
    fn candidates(directory: &Path, program: &str) -> Vec<PathBuf> {
        vec![directory.join(program)]
    }

    #[cfg(windows)]
    fn candidates(&self, directory: &Path, program: &str) -> Vec<PathBuf> {
        let requested = Path::new(program);
        if requested.extension().is_some() {
            return vec![directory.join(requested)];
        }
        self.path_extensions
            .iter()
            .map(|extension| directory.join(format!("{program}{extension}")))
            .collect()
    }
}

#[cfg(windows)]
fn controlled_path_extensions() -> Arc<[String]> {
    let allowed = [".exe", ".com", ".cmd", ".bat"];
    let configured = env::var("PATHEXT").unwrap_or_else(|_| allowed.join(";"));
    let mut extensions = Vec::new();
    for extension in configured.split(';') {
        let lower = extension.to_ascii_lowercase();
        if allowed.contains(&lower.as_str()) && !extensions.contains(&lower) {
            extensions.push(lower);
        }
    }
    if extensions.is_empty() {
        extensions.extend(allowed.map(str::to_owned));
    }
    extensions.into()
}

fn has_separator(program: &str) -> bool {
    program.contains('/') || program.contains('\\')
}

fn resolve_candidate(candidate: &Path) -> Result<ResolvedProgram, ProcessError> {
    let metadata = fs::metadata(candidate).map_err(|error| {
        ProcessError::Resolve(format!("cannot resolve {}: {error}", candidate.display()))
    })?;
    if !metadata.is_file() {
        return Err(ProcessError::Resolve(format!(
            "program is not a regular file: {}",
            candidate.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(ProcessError::Resolve(format!(
                "program is not executable: {}",
                candidate.display()
            )));
        }
    }
    let executable = fs::canonicalize(candidate).map_err(|error| {
        ProcessError::Resolve(format!("cannot normalize {}: {error}", candidate.display()))
    })?;
    let file_name = candidate.file_name().ok_or_else(|| {
        ProcessError::Resolve(format!(
            "program path has no executable name: {}",
            candidate.display()
        ))
    })?;
    let parent = candidate.parent().ok_or_else(|| {
        ProcessError::Resolve(format!(
            "program path has no parent directory: {}",
            candidate.display()
        ))
    })?;
    let absolute = fs::canonicalize(parent)
        .map_err(|error| {
            ProcessError::Resolve(format!(
                "cannot normalize program directory {}: {error}",
                parent.display()
            ))
        })?
        .join(file_name);
    let launcher = launcher_for(&executable)?;
    Ok(ResolvedProgram {
        absolute,
        executable,
        launcher,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Launcher {
    Native,
    #[cfg(windows)]
    CmdCompat,
}

impl Launcher {
    fn label(self) -> &'static str {
        match self {
            Self::Native => "native",
            #[cfg(windows)]
            Self::CmdCompat => "cmd-compat",
        }
    }
}

#[cfg(windows)]
fn launcher_for(path: &Path) -> Result<Launcher, ProcessError> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "exe" | "com" => Ok(Launcher::Native),
        "cmd" | "bat" => Ok(Launcher::CmdCompat),
        "ps1" => Err(ProcessError::Validation(
            ".ps1 requires a PowerShell launcher, which is not implemented".to_owned(),
        )),
        _ => Err(ProcessError::Resolve(format!(
            "unsupported Windows executable extension: .{extension}"
        ))),
    }
}

#[cfg(not(windows))]
fn launcher_for(path: &Path) -> Result<Launcher, ProcessError> {
    if path.as_os_str().is_empty() {
        return Err(ProcessError::Resolve("empty executable path".to_owned()));
    }
    Ok(Launcher::Native)
}

#[derive(Clone, Debug)]
struct ResolvedProgram {
    absolute: PathBuf,
    executable: PathBuf,
    launcher: Launcher,
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("invalid run_process request: {0}")]
    Validation(String),
    #[error("failed to resolve program: {0}")]
    Resolve(String),
    #[error("failed to launch or communicate with process: {0}")]
    Io(#[from] io::Error),
    #[error("{report}")]
    Timeout { timeout_ms: u64, report: String },
    #[error("process timed out after {timeout_ms} ms before spawn; no child was started")]
    TimeoutBeforeSpawn { timeout_ms: u64 },
    #[error("process was cancelled and its process tree was terminated")]
    Cancelled,
    #[error("process cleanup did not complete before its deadline; outcome uncertain")]
    OutcomeUncertain,
    #[error(transparent)]
    Output(#[from] crate::output::OutputError),
}

/// Resolve and execute one structured process request.
///
/// # Errors
///
/// Returns validation, resolution, spawn, I/O, cancellation, timeout, cleanup, or output errors.
pub fn execute(
    root: &Arc<RepositoryRoot>,
    resolver: &ProcessResolver,
    request: &ProcessRequest,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<String, ProcessError> {
    let started = std::time::Instant::now();
    request.validate()?;
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    let cwd = resolve_cwd(root, request.cwd.as_deref())?;
    let program = resolver.resolve(&request.program, &cwd)?;
    #[cfg(windows)]
    validate_launcher_request(&program, request)?;
    let timeout =
        timeout
            .checked_sub(started.elapsed())
            .ok_or(ProcessError::TimeoutBeforeSpawn {
                timeout_ms: request.timeout_ms(),
            })?;
    platform::run(&program, &cwd, request, timeout, cancellation)
}

fn resolve_cwd(root: &RepositoryRoot, requested: Option<&str>) -> Result<PathBuf, ProcessError> {
    let candidate = requested.map_or_else(|| root.path().to_owned(), PathBuf::from);
    let candidate = if candidate.is_absolute() {
        candidate
    } else {
        root.resolve(&candidate)
            .map_err(|error| ProcessError::Validation(format!("invalid relative cwd: {error}")))?
            .absolute()
            .to_owned()
    };
    let cwd = fs::canonicalize(&candidate).map_err(|error| {
        ProcessError::Validation(format!(
            "cannot resolve cwd {}: {error}",
            candidate.display()
        ))
    })?;
    if !cwd.is_dir() {
        return Err(ProcessError::Validation(format!(
            "cwd is not a directory: {}",
            cwd.display()
        )));
    }
    Ok(cwd)
}

#[cfg(windows)]
fn validate_launcher_request(
    resolved: &ResolvedProgram,
    request: &ProcessRequest,
) -> Result<(), ProcessError> {
    let file_name = resolved
        .executable
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if file_name.eq_ignore_ascii_case("cmd.exe")
        && request
            .args
            .iter()
            .any(|arg| arg.eq_ignore_ascii_case("/c") || arg.eq_ignore_ascii_case("/k"))
    {
        return Err(ProcessError::Validation(
            "direct cmd.exe /c or /k command evaluation is not accepted".to_owned(),
        ));
    }
    if (file_name.eq_ignore_ascii_case("powershell.exe")
        || file_name.eq_ignore_ascii_case("pwsh.exe"))
        && request
            .args
            .iter()
            .any(|arg| is_powershell_command_evaluation_arg(arg))
    {
        return Err(ProcessError::Validation(
            "PowerShell command-evaluation switches are not accepted".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn is_powershell_command_evaluation_arg(argument: &str) -> bool {
    let name = argument
        .split_once([':', '='])
        .map_or(argument, |(name, _)| name)
        .to_ascii_lowercase();
    if matches!(name.as_str(), "-cwa" | "-e" | "-ec") {
        return true;
    }
    ["-command", "-commandwithargs", "-encodedcommand"]
        .iter()
        .any(|full| name.len() > 1 && full.starts_with(&name))
}

#[cfg(unix)]
fn apply_environment(command: &mut std::process::Command, request: &ProcessRequest) {
    for (key, value) in ENVIRONMENT_DEFAULTS {
        command.env(key, value);
    }
    for key in &request.unset_env {
        command.env_remove(key);
    }
    for (key, value) in &request.env {
        command.env(key, value);
    }
}

#[derive(Debug)]
struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    bytes_read: usize,
}

impl Capture {
    fn new() -> Self {
        Self {
            head: Vec::with_capacity(CAPTURE_HEAD_BYTES),
            tail: VecDeque::with_capacity(CAPTURE_TAIL_BYTES),
            bytes_read: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes_read = self.bytes_read.saturating_add(bytes.len());
        let head_remaining = CAPTURE_HEAD_BYTES.saturating_sub(self.head.len());
        let head_bytes = head_remaining.min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_bytes]);
        for byte in &bytes[head_bytes..] {
            if self.tail.len() == CAPTURE_TAIL_BYTES {
                self.tail.pop_front();
            }
            self.tail.push_back(*byte);
        }
    }

    fn retained(&self) -> usize {
        self.head.len().saturating_add(self.tail.len())
    }

    fn dropped(&self) -> usize {
        self.bytes_read.saturating_sub(self.retained())
    }

    fn render(&self) -> RenderedCapture {
        let mut bytes = self.head.clone();
        if self.dropped() > 0 {
            bytes.extend_from_slice(
                format!("\n... {} bytes omitted ...\n", self.dropped()).as_bytes(),
            );
        }
        bytes.extend(self.tail.iter());
        let (text, invalid_bytes) = escape_invalid_utf8(&bytes);
        RenderedCapture {
            text,
            invalid_bytes,
        }
    }
}

struct RenderedCapture {
    text: String,
    invalid_bytes: usize,
}

fn escape_invalid_utf8(bytes: &[u8]) -> (String, usize) {
    let mut input = bytes;
    let mut output = String::new();
    let mut invalid = 0_usize;
    while !input.is_empty() {
        match std::str::from_utf8(input) {
            Ok(text) => {
                output.push_str(text);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                output.push_str(
                    std::str::from_utf8(&input[..valid])
                        .expect("valid_up_to always identifies valid UTF-8"),
                );
                let count = error
                    .error_len()
                    .unwrap_or(input.len().saturating_sub(valid));
                for byte in &input[valid..valid + count] {
                    use std::fmt::Write as _;
                    let _ = write!(output, "\\x{byte:02X}");
                    invalid += 1;
                }
                input = &input[valid + count..];
            }
        }
    }
    (output, invalid)
}

fn drain(mut reader: impl Read) -> io::Result<Capture> {
    let mut capture = Capture::new();
    let mut chunk = vec![0_u8; DRAIN_CHUNK_BYTES].into_boxed_slice();
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok(capture);
        }
        capture.push(&chunk[..count]);
    }
}

fn write_stdin(mut writer: impl Write, input: Option<&str>) -> io::Result<()> {
    if let Some(input) = input {
        writer.write_all(input.as_bytes())?;
    }
    Ok(())
}

fn spawn_monitored<T: Send + 'static>(
    failed: Arc<AtomicBool>,
    task: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> std::thread::JoinHandle<io::Result<T>> {
    std::thread::spawn(move || {
        let result = task();
        if result.is_err() {
            failed.store(true, Ordering::Release);
        }
        result
    })
}

struct CompletedProcess {
    resolved: ResolvedProgram,
    cwd: PathBuf,
    exit: String,
    duration: Duration,
    stdout: Capture,
    stderr: Capture,
}

struct TimedOutProcess {
    resolved: ResolvedProgram,
    cwd: PathBuf,
    duration: Duration,
    stdout: Capture,
    stderr: Capture,
}

fn render_completed(
    completed: &CompletedProcess,
    cancellation: &CancellationToken,
) -> Result<String, ProcessError> {
    let stdout = completed.stdout.render();
    let stderr = completed.stderr.render();
    let header = format!(
        "Resolved program: {}\nLauncher: {}\nCwd: {}",
        diagnostic_path(&completed.resolved.absolute),
        completed.resolved.launcher.label(),
        diagnostic_path(&completed.cwd)
    );
    let tail = vec![
        format!("Exit code: {}", completed.exit),
        format!("Duration ms: {}", completed.duration.as_millis()),
        format!(
            "Stdout bytes: read={}, retained={}, dropped={}, invalid={}",
            completed.stdout.bytes_read,
            completed.stdout.retained(),
            completed.stdout.dropped(),
            stdout.invalid_bytes
        ),
        format!(
            "Stderr bytes: read={}, retained={}, dropped={}, invalid={}",
            completed.stderr.bytes_read,
            completed.stderr.retained(),
            completed.stderr.dropped(),
            stderr.invalid_bytes
        ),
        "Complete.".to_owned(),
    ];
    let mut formatter = OutputFormatter::new(header, tail, OutputLimits::default())?;
    push_stream(&mut formatter, "stdout", &stdout.text, cancellation)?;
    push_stream(&mut formatter, "stderr", &stderr.text, cancellation)?;
    formatter.finish(cancellation).map_err(ProcessError::from)
}

fn render_timeout(timed_out: &TimedOutProcess, timeout_ms: u64) -> Result<String, ProcessError> {
    let stdout = timed_out.stdout.render();
    let stderr = timed_out.stderr.render();
    let header = format!(
        "process timed out after {timeout_ms} ms and its process tree was terminated\nResolved program: {}\nLauncher: {}\nCwd: {}\nStatus: timed out; process tree terminated",
        diagnostic_path(&timed_out.resolved.absolute),
        timed_out.resolved.launcher.label(),
        diagnostic_path(&timed_out.cwd)
    );
    let tail = vec![
        "Exit code: unavailable (timed out)".to_owned(),
        format!("Duration ms: {}", timed_out.duration.as_millis()),
        format!(
            "Stdout bytes: read={}, retained={}, dropped={}, invalid={}",
            timed_out.stdout.bytes_read,
            timed_out.stdout.retained(),
            timed_out.stdout.dropped(),
            stdout.invalid_bytes
        ),
        format!(
            "Stderr bytes: read={}, retained={}, dropped={}, invalid={}",
            timed_out.stderr.bytes_read,
            timed_out.stderr.retained(),
            timed_out.stderr.dropped(),
            stderr.invalid_bytes
        ),
        "Incomplete.".to_owned(),
    ];
    let cancellation = CancellationToken::new();
    let mut formatter = OutputFormatter::new(header, tail, OutputLimits::default())?;
    push_stream(&mut formatter, "stdout", &stdout.text, &cancellation)?;
    push_stream(&mut formatter, "stderr", &stderr.text, &cancellation)?;
    formatter.finish(&cancellation).map_err(ProcessError::from)
}

fn diagnostic_path(path: &Path) -> String {
    let rendered = path.display().to_string();
    if rendered.len() <= DIAGNOSTIC_PATH_BYTES {
        return rendered;
    }
    let retained = DIAGNOSTIC_PATH_BYTES - DIAGNOSTIC_PATH_MARKER.len();
    let mut head_end = retained / 2;
    while !rendered.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = rendered.len() - (retained - head_end);
    while !rendered.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}{}{}",
        &rendered[..head_end],
        DIAGNOSTIC_PATH_MARKER,
        &rendered[tail_start..]
    )
}

fn push_stream(
    formatter: &mut OutputFormatter,
    name: &str,
    text: &str,
    cancellation: &CancellationToken,
) -> Result<(), ProcessError> {
    if !formatter.try_push_line(format!("--- {name} ---"), cancellation)? {
        return Ok(());
    }
    for line in text.split('\n') {
        if !formatter.try_push_line(line, cancellation)? {
            break;
        }
    }
    Ok(())
}

#[cfg(unix)]
mod platform {
    use std::{
        io,
        os::unix::process::{CommandExt, ExitStatusExt},
        process::{Command, Stdio},
        thread,
    };

    use super::{
        CLEANUP_DEADLINE, Capture, CompletedProcess, Path, ProcessError, ProcessRequest,
        ResolvedProgram, TERM_GRACE, TimedOutProcess, apply_environment, drain, render_completed,
        render_timeout, spawn_monitored, write_stdin,
    };
    use std::sync::{Arc, atomic::Ordering};
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    pub(super) fn run(
        resolved: &ResolvedProgram,
        cwd: &Path,
        request: &ProcessRequest,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<String, ProcessError> {
        let started = Instant::now();
        let mut command = Command::new(&resolved.executable);
        command
            .arg0(&resolved.absolute)
            .args(&request.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_environment(&mut command, request);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let process_group = i32::try_from(child.id()).map_err(|_| {
            ProcessError::Io(io::Error::other("child process ID does not fit pid_t"))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("child stdin pipe was not created"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("child stdout pipe was not created"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("child stderr pipe was not created"))?;
        let input = request.stdin.clone();
        let io_failed = Arc::new(super::AtomicBool::new(false));
        let stdin_thread = spawn_monitored(Arc::clone(&io_failed), move || {
            write_stdin(stdin, input.as_deref())
        });
        let stdout_thread = spawn_monitored(Arc::clone(&io_failed), move || drain(stdout));
        let stderr_thread = spawn_monitored(Arc::clone(&io_failed), move || drain(stderr));

        let exit = loop {
            if io_failed.load(Ordering::Acquire) {
                terminate(process_group, &mut child)?;
                let (stdin_result, _, _) =
                    settle_threads(stdin_thread, stdout_thread, stderr_thread)?;
                stdin_result?;
                return Err(ProcessError::Io(io::Error::other(
                    "process I/O task failed without an error",
                )));
            }
            if cancellation.is_cancelled() {
                terminate(process_group, &mut child)?;
                let (stdin_result, _, _) =
                    settle_threads(stdin_thread, stdout_thread, stderr_thread)?;
                let _ = stdin_result;
                return Err(ProcessError::Cancelled);
            }
            if started.elapsed() >= timeout {
                terminate(process_group, &mut child)?;
                let (stdin_result, stdout, stderr) =
                    settle_threads(stdin_thread, stdout_thread, stderr_thread)?;
                let _ = stdin_result;
                let timeout_ms = request.timeout_ms();
                let report = render_timeout(
                    &TimedOutProcess {
                        resolved: resolved.clone(),
                        cwd: cwd.to_owned(),
                        duration: started.elapsed(),
                        stdout,
                        stderr,
                    },
                    timeout_ms,
                )?;
                return Err(ProcessError::Timeout { timeout_ms, report });
            }
            if let Some(status) = child.try_wait()? {
                if !group_exists(process_group)? {
                    break if let Some(code) = status.code() {
                        code.to_string()
                    } else {
                        format!("signal {}", status.signal().unwrap_or_default())
                    };
                }
            }
            thread::sleep(Duration::from_millis(10));
        };

        finish_completed(
            resolved,
            cwd,
            exit,
            started,
            (stdin_thread, stdout_thread, stderr_thread),
            cancellation,
        )
    }

    type PendingIo = (
        thread::JoinHandle<io::Result<()>>,
        thread::JoinHandle<io::Result<Capture>>,
        thread::JoinHandle<io::Result<Capture>>,
    );

    fn finish_completed(
        resolved: &ResolvedProgram,
        cwd: &Path,
        exit: String,
        started: Instant,
        (stdin, stdout, stderr): PendingIo,
        cancellation: &CancellationToken,
    ) -> Result<String, ProcessError> {
        let (stdin_result, stdout, stderr) = settle_threads(stdin, stdout, stderr)?;
        stdin_result?;
        render_completed(
            &CompletedProcess {
                resolved: resolved.clone(),
                cwd: cwd.to_owned(),
                exit,
                duration: started.elapsed(),
                stdout,
                stderr,
            },
            cancellation,
        )
    }

    type ThreadResults = (io::Result<()>, Capture, Capture);

    fn settle_threads(
        stdin: thread::JoinHandle<io::Result<()>>,
        stdout: thread::JoinHandle<io::Result<Capture>>,
        stderr: thread::JoinHandle<io::Result<Capture>>,
    ) -> Result<ThreadResults, ProcessError> {
        let started = Instant::now();
        while !(stdin.is_finished() && stdout.is_finished() && stderr.is_finished()) {
            if started.elapsed() >= CLEANUP_DEADLINE {
                return Err(ProcessError::OutcomeUncertain);
            }
            thread::sleep(Duration::from_millis(10));
        }
        let stdin = stdin
            .join()
            .map_err(|_| io::Error::other("stdin writer panicked"))?;
        let stdout = stdout
            .join()
            .map_err(|_| io::Error::other("stdout drainer panicked"))??;
        let stderr = stderr
            .join()
            .map_err(|_| io::Error::other("stderr drainer panicked"))??;
        Ok((stdin, stdout, stderr))
    }

    fn terminate(process_group: i32, child: &mut std::process::Child) -> Result<(), ProcessError> {
        signal_group(process_group, libc::SIGTERM)?;
        let grace = Instant::now();
        while grace.elapsed() < TERM_GRACE {
            let _ = child.try_wait()?;
            if !group_exists(process_group)? {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        signal_group(process_group, libc::SIGKILL)?;
        let cleanup = Instant::now();
        while cleanup.elapsed() < CLEANUP_DEADLINE {
            let _ = child.try_wait()?;
            if !group_exists(process_group)? {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(ProcessError::OutcomeUncertain)
    }

    fn signal_group(process_group: i32, signal: i32) -> io::Result<()> {
        let result = unsafe { libc::kill(-process_group, signal) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn group_exists(process_group: i32) -> io::Result<bool> {
        let result = unsafe { libc::kill(-process_group, 0) };
        if result == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Ok(true),
            _ => Err(error),
        }
    }
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs;
    #[cfg(windows)]
    use std::{env, process::Command, thread};
    #[cfg(any(unix, windows))]
    use std::{sync::Arc, time::Duration};

    #[cfg(any(unix, windows))]
    use crate::path::RepositoryRoot;

    fn request(program: String) -> ProcessRequest {
        ProcessRequest {
            program,
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            unset_env: Vec::new(),
            stdin: None,
            timeout_ms: Some(2_000),
        }
    }

    #[test]
    fn validation_rejects_conflicts_nul_and_oversized_stdin() {
        let mut invalid = request("tool".to_owned());
        invalid.env.insert("Path".to_owned(), "value".to_owned());
        invalid.unset_env.push("Path".to_owned());
        assert!(matches!(
            invalid.validate(),
            Err(ProcessError::Validation(_))
        ));

        invalid.env.clear();
        invalid.unset_env.clear();
        invalid.args.push("nul\0arg".to_owned());
        assert!(matches!(
            invalid.validate(),
            Err(ProcessError::Validation(_))
        ));

        invalid.args.clear();
        invalid.stdin = Some("x".repeat(MAX_STDIN_BYTES + 1));
        assert!(matches!(
            invalid.validate(),
            Err(ProcessError::Validation(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn powershell_command_evaluation_switches_are_classified_conservatively() {
        for denied in [
            "-Command",
            "-c",
            "-command:Get-Process",
            "-CommandWithArgs",
            "-cwa",
            "-EncodedCommand",
            "-e",
            "-ec",
            "-enc",
            "-encodedcommand=payload",
        ] {
            assert!(
                is_powershell_command_evaluation_arg(denied),
                "{denied} must be rejected"
            );
        }
        for allowed in [
            "-ConfigurationName",
            "-EncodedArguments",
            "-ExecutionPolicy",
            "-File",
            "-NoProfile",
        ] {
            assert!(
                !is_powershell_command_evaluation_arg(allowed),
                "{allowed} is not a command-evaluation switch"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolver_ignores_empty_path_and_requires_executable_regular_file() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().expect("fixture");
        let executable = fixture.path().join("probe");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("write probe");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).expect("chmod");
        let resolver = ProcessResolver::for_tests(vec![fixture.path().to_owned()]);
        let program = resolver.resolve("probe", fixture.path()).expect("resolve");
        let executable = fs::canonicalize(executable).expect("canonical");
        assert_eq!(program.absolute, executable);
        assert_eq!(program.executable, executable);
        assert!(resolver.resolve("probe arg", fixture.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unix_multicall_proxy_preserves_resolved_argv0() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().expect("fixture");
        let proxy = fixture.path().join("cargo");
        symlink(std::env::current_exe().expect("test executable"), &proxy)
            .expect("create multicall proxy");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let resolver = ProcessResolver::for_tests(vec![fixture.path().to_owned()]);
        let mut proxy_request = request("cargo".to_owned());
        proxy_request.args = vec![
            "--exact".to_owned(),
            "tools::process::tests::unix_multicall_argv0_child_fixture".to_owned(),
            "--nocapture".to_owned(),
        ];
        proxy_request
            .env
            .insert("CODEXSHIM_MULTICALL_FIXTURE".to_owned(), "child".to_owned());

        let output = execute(
            &root,
            &resolver,
            &proxy_request,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .expect("multicall proxy");

        assert!(output.contains(&format!("Resolved program: {}", proxy.display())));
        assert!(output.contains("multicall argv0: cargo"));
        assert!(output.contains("Exit code: 0"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_multicall_argv0_child_fixture() {
        if std::env::var("CODEXSHIM_MULTICALL_FIXTURE").as_deref() != Ok("child") {
            return;
        }
        let argv0 = std::env::args_os().next().expect("argv0");
        let name = Path::new(&argv0)
            .file_name()
            .and_then(|value| value.to_str())
            .expect("UTF-8 argv0 name");
        assert_eq!(name, "cargo");
        println!("multicall argv0: {name}");
    }

    #[test]
    fn invalid_utf8_is_escaped_across_valid_spans() {
        let (rendered, invalid) = escape_invalid_utf8(b"a\xF0\x9F\x92\xA9b\xFFc\xE2\x82");
        assert_eq!(rendered, "a💩b\\xFFc\\xE2\\x82");
        assert_eq!(invalid, 3);

        let mut capture = Capture::new();
        capture.push(b"a\xF0\x9F");
        capture.push(b"\x92\xA9b\xFF");
        let rendered = capture.render();
        assert_eq!(rendered.text, "a💩b\\xFF");
        assert_eq!(rendered.invalid_bytes, 1);
    }

    #[test]
    fn timeout_report_is_bounded_and_preserves_required_diagnostics() {
        let mut stdout = Capture::new();
        stdout.push(&vec![b'o'; crate::output::MODEL_BYTE_LIMIT * 2]);
        let mut stderr = Capture::new();
        stderr.push(b"timeout stderr evidence\n");
        let report = render_timeout(
            &TimedOutProcess {
                resolved: ResolvedProgram {
                    absolute: PathBuf::from("cargo"),
                    executable: PathBuf::from("cargo"),
                    launcher: Launcher::Native,
                },
                cwd: PathBuf::from("workspace"),
                duration: Duration::from_millis(150),
                stdout,
                stderr,
            },
            150,
        )
        .expect("timeout report");

        assert!(report.contains("Resolved program: cargo"));
        assert!(report.contains("Launcher: native"));
        assert!(report.contains("Cwd: workspace"));
        assert!(report.contains("Exit code: unavailable (timed out)"));
        assert!(report.contains("timeout stderr evidence"));
        assert!(report.ends_with("Incomplete."));
        assert!(report.len() <= crate::output::MODEL_BYTE_LIMIT);
        assert!(crate::output::token_count(&report) <= crate::output::MODEL_TOKEN_LIMIT);
    }

    #[test]
    fn before_spawn_timeout_does_not_claim_process_diagnostics() {
        let message = ProcessError::TimeoutBeforeSpawn { timeout_ms: 25 }.to_string();
        assert!(message.contains("no child was started"));
        for field in ["Resolved program:", "Launcher:", "Cwd:", "Exit code:"] {
            assert!(!message.contains(field));
        }
    }

    #[test]
    fn capture_keeps_bounded_head_and_tail_while_counting_all_bytes() {
        let bytes = vec![b'x'; CAPTURE_HEAD_BYTES + CAPTURE_TAIL_BYTES + 17];
        let mut capture = Capture::new();
        capture.push(&bytes);
        assert_eq!(capture.bytes_read, bytes.len());
        assert_eq!(capture.retained(), CAPTURE_HEAD_BYTES + CAPTURE_TAIL_BYTES);
        assert_eq!(capture.dropped(), 17);
    }

    #[cfg(unix)]
    fn execute_unix(request: &ProcessRequest) -> Result<String, ProcessError> {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        execute(
            &root,
            &ProcessResolver::capture(),
            request,
            Duration::from_millis(request.timeout_ms()),
            &CancellationToken::new(),
        )
    }

    #[cfg(unix)]
    #[test]
    fn unix_native_argv_nonzero_exit_and_environment_are_reported() {
        let mut printf = request("/usr/bin/printf".to_owned());
        printf.args = vec!["[%s]\n".to_owned(), "a b".to_owned(), "&|$".to_owned()];
        let output = execute_unix(&printf).expect("printf");
        assert!(output.contains("[a b]\n[&|$]"));
        assert!(output.contains("Launcher: native"));
        assert!(output.contains("Exit code: 0"));

        let mut nonzero = request("/bin/sh".to_owned());
        nonzero.args = vec!["-c".to_owned(), "exit 7".to_owned()];
        let output = execute_unix(&nonzero).expect("nonzero is a completed result");
        assert!(output.contains("Exit code: 7"));

        let mut environment = request("/usr/bin/env".to_owned());
        environment
            .env
            .insert("CODEXSHIM_PROBE".to_owned(), "set".to_owned());
        let output = execute_unix(&environment).expect("environment");
        assert!(output.contains("NO_COLOR=1"));
        assert!(output.contains("CODEXSHIM_PROBE=set"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_python_node_and_git_receive_literal_argument_corpus() {
        let corpus = vec!["", "a b", "q\"r", "\\", "界", "&|<>^%!"];
        let expected = serde_json::to_string(&corpus).expect("expected JSON");

        let mut python = request("python3".to_owned());
        python.args = vec![
            "-c".to_owned(),
            "import json,sys; print(json.dumps(sys.argv[1:], ensure_ascii=False, separators=(',', ':')))"
                .to_owned(),
        ];
        python.args.extend(corpus.iter().map(ToString::to_string));
        let output = execute_unix(&python).expect("Python argv probe");
        assert!(output.contains(&expected));

        let mut node = request("node".to_owned());
        node.args = vec![
            "-e".to_owned(),
            "console.log(JSON.stringify(process.argv.slice(1)))".to_owned(),
        ];
        node.args.extend(corpus.iter().map(ToString::to_string));
        let output = execute_unix(&node).expect("Node argv probe");
        assert!(output.contains(&expected));

        let mut git = request("git".to_owned());
        git.args = vec![
            "rev-parse".to_owned(),
            "--sq-quote".to_owned(),
            "a b&|.tmp".to_owned(),
        ];
        let output = execute_unix(&git).expect("Git argv probe");
        assert!(output.contains("a b&|.tmp"));
        assert!(output.contains("Exit code: 0"));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_absolute_cwd_may_leave_root_but_relative_escape_is_rejected() {
        let fixture = tempfile::tempdir().expect("root fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let mut absolute = request("/usr/bin/printf".to_owned());
        absolute.args = vec!["cwd".to_owned()];
        absolute.cwd = Some(outside.path().to_string_lossy().into_owned());
        let output = execute(
            &root,
            &ProcessResolver::for_tests(Vec::new()),
            &absolute,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .expect("absolute cwd outside root");
        assert!(output.contains(&format!("Cwd: {}", outside.path().display())));

        absolute.cwd = Some("../outside".to_owned());
        assert!(matches!(
            execute(
                &root,
                &ProcessResolver::for_tests(Vec::new()),
                &absolute,
                Duration::from_secs(5),
                &CancellationToken::new(),
            ),
            Err(ProcessError::Validation(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unix_large_stdin_and_both_output_streams_do_not_deadlock() {
        let mut high_output = request("/bin/sh".to_owned());
        high_output.timeout_ms = Some(10_000);
        high_output.stdin = Some("i".repeat(MAX_STDIN_BYTES));
        high_output.args = vec![
            "-c".to_owned(),
            "cat >/dev/null; i=0; while [ $i -lt 4000 ]; do printf 'stdout-%04d-xxxxxxxxxxxxxxxx\n' \"$i\"; printf 'stderr-%04d-yyyyyyyyyyyyyyyy\n' \"$i\" >&2; i=$((i+1)); done".to_owned(),
        ];
        let output = execute_unix(&high_output).expect("high output");
        assert!(output.contains("Exit code: 0"));
        assert!(output.contains("bytes omitted"));
        assert!(output.contains("dropped="));
        assert!(output.len() <= crate::output::MODEL_BYTE_LIMIT);
    }

    #[cfg(unix)]
    #[test]
    fn unix_timeout_terminates_descendant_process_group() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let pid_file = fixture.path().join("descendant.pid");
        let mut timed = request("/bin/sh".to_owned());
        timed.timeout_ms = Some(150);
        timed.args = vec![
            "-c".to_owned(),
            format!(
                "printf 'timeout stdout evidence\\n'; printf 'timeout stderr evidence\\n' >&2; sleep 30 & echo $! > '{}'; wait",
                pid_file.display()
            ),
        ];
        let resolver = ProcessResolver::for_tests(Vec::new());
        let resolved_shell = resolver
            .resolve("/bin/sh", root.path())
            .expect("resolve shell fixture");
        let error = execute(
            &root,
            &resolver,
            &timed,
            Duration::from_millis(150),
            &CancellationToken::new(),
        )
        .expect_err("timeout");
        assert!(
            matches!(&error, ProcessError::Timeout { .. }),
            "unexpected process error: {error}"
        );
        let report = error.to_string();
        assert!(report.contains(&format!(
            "Resolved program: {}",
            resolved_shell.absolute.display()
        )));
        assert!(report.contains("Cwd:"));
        assert!(report.contains("timeout stdout evidence"));
        assert!(report.contains("timeout stderr evidence"));
        assert!(report.contains("Exit code: unavailable (timed out)"));
        assert!(report.ends_with("Incomplete."));
        assert!(report.len() <= crate::output::MODEL_BYTE_LIMIT);
        let pid = fs::read_to_string(pid_file)
            .expect("descendant pid")
            .trim()
            .parse::<i32>()
            .expect("pid integer");
        let result = unsafe { libc::kill(pid, 0) };
        assert_eq!(result, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[cfg(unix)]
    #[test]
    fn unix_cancellation_terminates_running_process() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let mut running = request("/bin/sh".to_owned());
        running.timeout_ms = Some(5_000);
        running.args = vec!["-c".to_owned(), "sleep 30".to_owned()];
        let cancellation = CancellationToken::new();
        let trigger = cancellation.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            trigger.cancel();
        });
        let error = execute(
            &root,
            &ProcessResolver::for_tests(Vec::new()),
            &running,
            Duration::from_secs(5),
            &cancellation,
        )
        .expect_err("cancelled");
        canceller.join().expect("canceller");
        assert!(matches!(error, ProcessError::Cancelled));
    }

    #[cfg(unix)]
    #[test]
    fn unix_stdin_failure_terminates_process_tree_immediately() {
        let mut request = request("/bin/sh".to_owned());
        request.timeout_ms = Some(10_000);
        request.stdin = Some("i".repeat(MAX_STDIN_BYTES));
        request.args = vec!["-c".to_owned(), "exec 0<&-; sleep 30".to_owned()];
        let started = std::time::Instant::now();

        let error = execute_unix(&request).expect_err("stdin failure");

        assert!(matches!(error, ProcessError::Io(_)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(windows)]
    #[test]
    fn windows_grandchild_child_fixture() {
        if env::var("CODEXSHIM_PROCESS_FIXTURE").as_deref() != Ok("child") {
            return;
        }
        let pid_file = env::var_os("CODEXSHIM_PROCESS_PID_FILE").expect("pid file");
        std::fs::write(pid_file, std::process::id().to_string()).expect("write child pid");
        thread::sleep(Duration::from_secs(30));
    }

    #[cfg(windows)]
    #[test]
    fn windows_grandchild_parent_fixture() {
        use std::io::Write as _;

        if env::var("CODEXSHIM_PROCESS_FIXTURE").as_deref() != Ok("parent") {
            return;
        }
        writeln!(std::io::stdout(), "timeout stdout evidence").expect("write stdout evidence");
        std::io::stdout().flush().expect("flush stdout evidence");
        writeln!(std::io::stderr(), "timeout stderr evidence").expect("write stderr evidence");
        std::io::stderr().flush().expect("flush stderr evidence");
        let executable = env::current_exe().expect("test executable");
        let status = Command::new(executable)
            .args([
                "--exact",
                "tools::process::tests::windows_grandchild_child_fixture",
                "--nocapture",
            ])
            .env("CODEXSHIM_PROCESS_FIXTURE", "child")
            .env(
                "CODEXSHIM_PROCESS_PID_FILE",
                env::var_os("CODEXSHIM_PROCESS_PID_FILE").expect("pid file"),
            )
            .status()
            .expect("spawn child fixture");
        assert!(status.success());
    }

    #[cfg(windows)]
    #[test]
    fn windows_lingering_grandchild_parent_fixture() {
        if env::var("CODEXSHIM_PROCESS_FIXTURE").as_deref() != Ok("lingering-parent") {
            return;
        }
        let pid_file = env::var_os("CODEXSHIM_PROCESS_PID_FILE").expect("pid file");
        let executable = env::current_exe().expect("test executable");
        let child = Command::new(executable)
            .args([
                "--exact",
                "tools::process::tests::windows_grandchild_child_fixture",
                "--nocapture",
            ])
            .env("CODEXSHIM_PROCESS_FIXTURE", "child")
            .env("CODEXSHIM_PROCESS_PID_FILE", &pid_file)
            .spawn()
            .expect("spawn lingering child fixture");
        let pid_file = std::path::PathBuf::from(pid_file);
        let started = std::time::Instant::now();
        while !pid_file.exists() && started.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(pid_file.exists(), "lingering child did not start");
        drop(child);
    }

    #[cfg(windows)]
    fn windows_process_is_running(pid: u32) -> bool {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };

        const STILL_ACTIVE_EXIT_CODE: u32 = 259;
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0_u32;
        let succeeded = unsafe { GetExitCodeProcess(handle, &raw mut exit_code) } != 0;
        unsafe {
            CloseHandle(handle);
        }
        succeeded && exit_code == STILL_ACTIVE_EXIT_CODE
    }

    #[cfg(windows)]
    #[test]
    fn windows_primary_exit_terminates_lingering_grandchild() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let pid_file = fixture.path().join("lingering-grandchild.pid");
        let executable = env::current_exe().expect("test executable");
        let mut request = request(executable.to_string_lossy().into_owned());
        request.args = vec![
            "--exact".to_owned(),
            "tools::process::tests::windows_lingering_grandchild_parent_fixture".to_owned(),
            "--nocapture".to_owned(),
        ];
        request.env.insert(
            "CODEXSHIM_PROCESS_FIXTURE".to_owned(),
            "lingering-parent".to_owned(),
        );
        request.env.insert(
            "CODEXSHIM_PROCESS_PID_FILE".to_owned(),
            pid_file.to_string_lossy().into_owned(),
        );
        request.timeout_ms = Some(5_000);
        let started = std::time::Instant::now();

        let output = execute(
            &root,
            &ProcessResolver::capture(),
            &request,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .expect("completed primary process");

        assert!(output.contains("Exit code: 0"));
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid = std::fs::read_to_string(pid_file)
            .expect("lingering child pid")
            .trim()
            .parse::<u32>()
            .expect("pid integer");
        assert!(
            !windows_process_is_running(pid),
            "lingering grandchild survived primary completion"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_timeout_terminates_grandchild_job_tree() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let pid_file = fixture.path().join("grandchild.pid");
        let executable = env::current_exe().expect("test executable");
        let mut timed = request(executable.to_string_lossy().into_owned());
        timed.args = vec![
            "--exact".to_owned(),
            "tools::process::tests::windows_grandchild_parent_fixture".to_owned(),
            "--nocapture".to_owned(),
        ];
        timed
            .env
            .insert("CODEXSHIM_PROCESS_FIXTURE".to_owned(), "parent".to_owned());
        timed.env.insert(
            "CODEXSHIM_PROCESS_PID_FILE".to_owned(),
            pid_file.to_string_lossy().into_owned(),
        );
        timed.timeout_ms = Some(750);
        let error = execute(
            &root,
            &ProcessResolver::capture(),
            &timed,
            Duration::from_millis(750),
            &CancellationToken::new(),
        )
        .expect_err("timeout");
        assert!(
            matches!(&error, ProcessError::Timeout { .. }),
            "unexpected process error: {error}"
        );
        let report = error.to_string();
        assert!(report.contains("Resolved program:"));
        assert!(report.contains("Launcher: native"));
        assert!(report.contains("Cwd:"));
        assert!(report.contains("timeout stdout evidence"));
        assert!(report.contains("timeout stderr evidence"));
        assert!(report.contains("Exit code: unavailable (timed out)"));
        assert!(report.ends_with("Incomplete."));
        assert!(report.len() <= crate::output::MODEL_BYTE_LIMIT);
        let pid = std::fs::read_to_string(pid_file)
            .expect("grandchild pid")
            .trim()
            .parse::<u32>()
            .expect("pid integer");
        assert!(
            !windows_process_is_running(pid),
            "grandchild process survived job termination"
        );
    }
}
