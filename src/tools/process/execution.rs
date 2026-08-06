#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("invalid run_process request: {0}")]
    Validation(String),
    #[error("failed to resolve program: {0}")]
    Resolve(String),
    #[error("failed to launch or communicate with process: {0}")]
    Io(#[from] io::Error),
    #[error("{report}")]
    Timeout {
        timeout_ms: u64,
        report: String,
        details: Box<ProcessTimeoutDetails>,
    },
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
    execute_output(root, resolver, request, timeout, cancellation).map(|result| result.text)
}

pub(crate) fn execute_output(
    root: &Arc<RepositoryRoot>,
    resolver: &ProcessResolver,
    request: &ProcessRequest,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ProcessError> {
    execute_inner(root, resolver, request, timeout, cancellation)
}

fn execute_inner(
    root: &Arc<RepositoryRoot>,
    resolver: &ProcessResolver,
    request: &ProcessRequest,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ProcessError> {
    let started = std::time::Instant::now();
    request.validate()?;
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    tracing::info!(target: "codexshim", event = "process_resolve", phase = "execution");
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
    tracing::info!(target: "codexshim", event = "process_spawn", phase = "execution");
    let result = platform::run(&program, &cwd, request, timeout, cancellation);
    match &result {
        Ok(_) => tracing::info!(target: "codexshim", event = "process_exit", phase = "execution"),
        Err(ProcessError::Timeout { .. } | ProcessError::TimeoutBeforeSpawn { .. }) => {
            tracing::error!(target: "codexshim", event = "process_timeout", phase = "cleanup", error_class = "resource_timeout");
        }
        Err(ProcessError::OutcomeUncertain) => {
            tracing::error!(target: "codexshim", event = "process_cleanup", phase = "cleanup", outcome = "uncertain", error_class = "outcome_uncertain");
        }
        Err(_) => {}
    }
    result
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
    tail: Vec<u8>,
    tail_start: usize,
    bytes_read: usize,
}

impl Capture {
    fn new() -> Self {
        Self {
            head: Vec::with_capacity(CAPTURE_HEAD_BYTES),
            tail: Vec::with_capacity(CAPTURE_TAIL_BYTES),
            tail_start: 0,
            bytes_read: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes_read = self.bytes_read.saturating_add(bytes.len());
        let head_remaining = CAPTURE_HEAD_BYTES.saturating_sub(self.head.len());
        let head_bytes = head_remaining.min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_bytes]);
        self.push_tail(&bytes[head_bytes..]);
    }

    fn push_tail(&mut self, bytes: &[u8]) {
        if bytes.len() >= CAPTURE_TAIL_BYTES {
            self.tail.clear();
            self.tail
                .extend_from_slice(&bytes[bytes.len() - CAPTURE_TAIL_BYTES..]);
            self.tail_start = 0;
            return;
        }
        if self.tail.len() < CAPTURE_TAIL_BYTES {
            let appended = bytes.len().min(CAPTURE_TAIL_BYTES - self.tail.len());
            self.tail.extend_from_slice(&bytes[..appended]);
            if appended == bytes.len() {
                return;
            }
            self.overwrite_tail(&bytes[appended..]);
            return;
        }
        self.overwrite_tail(bytes);
    }

    fn overwrite_tail(&mut self, bytes: &[u8]) {
        let first = bytes.len().min(CAPTURE_TAIL_BYTES - self.tail_start);
        self.tail[self.tail_start..self.tail_start + first].copy_from_slice(&bytes[..first]);
        self.tail[..bytes.len() - first].copy_from_slice(&bytes[first..]);
        self.tail_start = (self.tail_start + bytes.len()) % CAPTURE_TAIL_BYTES;
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
        if self.tail.len() < CAPTURE_TAIL_BYTES || self.tail_start == 0 {
            bytes.extend_from_slice(&self.tail);
        } else {
            bytes.extend_from_slice(&self.tail[self.tail_start..]);
            bytes.extend_from_slice(&self.tail[..self.tail_start]);
        }
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

#[cfg(windows)]
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

#[cfg(windows)]
fn write_stdin(mut writer: impl Write, input: Option<&str>) -> io::Result<()> {
    if let Some(input) = input {
        writer.write_all(input.as_bytes())?;
    }
    Ok(())
}

#[cfg(windows)]
fn spawn_monitored<T: Send + 'static>(
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
struct ThreadCompletion {
    state: Arc<(Mutex<usize>, Condvar)>,
}

#[cfg(windows)]
impl ThreadCompletion {
    fn new() -> Self {
        Self {
            state: Arc::new((Mutex::new(0), Condvar::new())),
        }
    }

    fn signal_on_drop(&self) -> CompletionSignal {
        CompletionSignal(self.clone())
    }

    fn wait_for(&self, count: usize, timeout: Duration) -> bool {
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
) -> Result<ToolOutput, ProcessError> {
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
    let rendered = formatter.finish(cancellation)?;
    let result = ProcessResult {
        program: diagnostic_path(&completed.resolved.absolute),
        cwd: diagnostic_path(&completed.cwd),
        launcher: completed.resolved.launcher.label().to_owned(),
        exit_code: completed.exit.clone(),
        duration_ms: u64::try_from(completed.duration.as_millis()).unwrap_or(u64::MAX),
        stdout: ProcessCaptureResult {
            text: stdout.text,
            total_bytes: completed.stdout.bytes_read,
            retained_bytes: completed.stdout.retained(),
            dropped_bytes: completed.stdout.dropped(),
            invalid_utf8_bytes: stdout.invalid_bytes,
        },
        stderr: ProcessCaptureResult {
            text: stderr.text,
            total_bytes: completed.stderr.bytes_read,
            retained_bytes: completed.stderr.retained(),
            dropped_bytes: completed.stderr.dropped(),
            invalid_utf8_bytes: stderr.invalid_bytes,
        },
    };
    let child_nonzero = result.exit_code != "0";
    let output = ToolOutput::process(rendered, &result, child_nonzero)?;
    if !output.fits_budget() {
        return Err(crate::output::OutputError::InvariantViolation.into());
    }
    Ok(output)
}

fn render_timeout(
    timed_out: &TimedOutProcess,
    timeout_ms: u64,
) -> Result<TimeoutRender, ProcessError> {
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
    let text = formatter.finish(&cancellation)?;
    Ok(TimeoutRender {
        text,
        details: ProcessTimeoutDetails {
            timeout_ms,
            program: diagnostic_path(&timed_out.resolved.absolute),
            cwd: diagnostic_path(&timed_out.cwd),
            launcher: timed_out.resolved.launcher.label().to_owned(),
            duration_ms: u64::try_from(timed_out.duration.as_millis()).unwrap_or(u64::MAX),
            stdout: ProcessCaptureResult {
                text: stdout.text,
                total_bytes: timed_out.stdout.bytes_read,
                retained_bytes: timed_out.stdout.retained(),
                dropped_bytes: timed_out.stdout.dropped(),
                invalid_utf8_bytes: stdout.invalid_bytes,
            },
            stderr: ProcessCaptureResult {
                text: stderr.text,
                total_bytes: timed_out.stderr.bytes_read,
                retained_bytes: timed_out.stderr.retained(),
                dropped_bytes: timed_out.stderr.dropped(),
                invalid_utf8_bytes: stderr.invalid_bytes,
            },
            termination_outcome: "terminated",
        },
    })
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
