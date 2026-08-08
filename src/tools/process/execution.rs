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

    fn render(&self, limit: usize) -> RenderedCapture {
        let limit = limit.min(self.retained()).min(self.bytes_read);
        if limit == self.bytes_read && self.dropped() == 0 {
            let mut bytes = self.head.clone();
            bytes.extend_from_slice(&self.ordered_tail());
            let (text, invalid_bytes) = escape_invalid_utf8(&bytes);
            return RenderedCapture {
                text,
                shown_bytes: self.bytes_read,
                omitted_bytes: 0,
                invalid_bytes,
            };
        }

        let ordered_tail = self.ordered_tail();
        let contiguous;
        let (head_source, tail_source) = if self.dropped() == 0 {
            contiguous = {
                let mut bytes = self.head.clone();
                bytes.extend_from_slice(&ordered_tail);
                bytes
            };
            (contiguous.as_slice(), contiguous.as_slice())
        } else {
            (self.head.as_slice(), ordered_tail.as_slice())
        };
        let (head_count, tail_count) =
            allocate_view_bytes(limit, head_source.len(), tail_source.len());
        let head = align_head(&head_source[..head_count]);
        let tail = align_tail(&tail_source[tail_source.len().saturating_sub(tail_count)..]);
        let shown_bytes = head.len().saturating_add(tail.len());
        let omitted_bytes = self.bytes_read.saturating_sub(shown_bytes);
        let mut bytes = Vec::with_capacity(
            shown_bytes
                .saturating_add(64)
                .min(crate::output::MODEL_BYTE_LIMIT),
        );
        bytes.extend_from_slice(head);
        if omitted_bytes > 0 {
            if bytes.last().is_some_and(|byte| *byte != b'\n') {
                bytes.push(b'\n');
            }
            bytes.extend_from_slice(format!("... {omitted_bytes} bytes omitted ...").as_bytes());
            if !tail.is_empty() && tail.first().is_some_and(|byte| *byte != b'\n') {
                bytes.push(b'\n');
            }
        }
        bytes.extend_from_slice(tail);
        let (text, invalid_bytes) = escape_invalid_utf8(&bytes);
        RenderedCapture {
            text,
            shown_bytes,
            omitted_bytes,
            invalid_bytes,
        }
    }

    fn ordered_tail(&self) -> Vec<u8> {
        if self.tail.len() < CAPTURE_TAIL_BYTES || self.tail_start == 0 {
            return self.tail.clone();
        }
        let mut ordered = Vec::with_capacity(self.tail.len());
        ordered.extend_from_slice(&self.tail[self.tail_start..]);
        ordered.extend_from_slice(&self.tail[..self.tail_start]);
        ordered
    }
}

struct RenderedCapture {
    text: String,
    shown_bytes: usize,
    omitted_bytes: usize,
    invalid_bytes: usize,
}

fn allocate_view_bytes(limit: usize, head_available: usize, tail_available: usize) -> (usize, usize) {
    let mut head = limit.div_ceil(2).min(head_available);
    let mut tail = (limit / 2).min(tail_available);
    let mut remaining = limit.saturating_sub(head).saturating_sub(tail);
    let extra_head = remaining.min(head_available.saturating_sub(head));
    head += extra_head;
    remaining -= extra_head;
    tail += remaining.min(tail_available.saturating_sub(tail));
    (head, tail)
}

fn align_head(bytes: &[u8]) -> &[u8] {
    let clipped = &bytes[..trim_incomplete_utf8_suffix(bytes)];
    if let Some(end) = clipped.iter().rposition(|byte| *byte == b'\n') {
        let aligned = &clipped[..=end];
        if aligned.len() >= clipped.len().div_ceil(2) {
            return aligned;
        }
    }
    clipped
}

fn align_tail(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len() && is_utf8_continuation(bytes[start]) {
        start += 1;
    }
    let clipped = &bytes[start..];
    if let Some(end) = clipped.iter().position(|byte| *byte == b'\n') {
        let aligned = &clipped[end + 1..];
        if aligned.len() >= clipped.len().div_ceil(2) {
            return aligned;
        }
    }
    clipped
}

fn trim_incomplete_utf8_suffix(bytes: &[u8]) -> usize {
    let end = bytes.len();
    let mut lead = end;
    while lead > 0 && is_utf8_continuation(bytes[lead - 1]) && end - lead < 3 {
        lead -= 1;
    }
    if lead == 0 {
        return end;
    }
    let first = bytes[lead - 1];
    let width = match first {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return end,
    };
    let sequence_start = lead - 1;
    if end - sequence_start < width {
        sequence_start
    } else {
        end
    }
}

fn is_utf8_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
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
    project_captures(
        &completed.stdout,
        &completed.stderr,
        cancellation,
        |stdout, stderr| completed_output(completed, stdout, stderr),
        ToolOutput::fits_budget,
    )
}

fn completed_output(
    completed: &CompletedProcess,
    stdout: &RenderedCapture,
    stderr: &RenderedCapture,
) -> ToolOutput {
    let header = format!(
        "Resolved program: {}\nLauncher: {}\nCwd: {}",
        diagnostic_path(&completed.resolved.absolute),
        completed.resolved.launcher.label(),
        diagnostic_path(&completed.cwd)
    );
    let tail = [
        format!("Exit code: {}", completed.exit),
        format!("Duration ms: {}", completed.duration.as_millis()),
        format!(
            "Stdout bytes: total={}, shown={}, omitted={}, invalid={}",
            completed.stdout.bytes_read,
            stdout.shown_bytes,
            stdout.omitted_bytes,
            stdout.invalid_bytes
        ),
        format!(
            "Stderr bytes: total={}, shown={}, omitted={}, invalid={}",
            completed.stderr.bytes_read,
            stderr.shown_bytes,
            stderr.omitted_bytes,
            stderr.invalid_bytes
        ),
        "Complete.".to_owned(),
    ];
    let mut rendered = String::with_capacity(
        header
            .len()
            .saturating_add(stdout.text.len())
            .saturating_add(stderr.text.len())
            .saturating_add(256),
    );
    rendered.push_str(&header);
    rendered.push_str("\n--- stdout ---\n");
    rendered.push_str(&stdout.text);
    rendered.push_str("\n--- stderr ---\n");
    rendered.push_str(&stderr.text);
    for line in tail {
        rendered.push('\n');
        rendered.push_str(&line);
    }
    ToolOutput::with_child_nonzero(rendered, completed.exit != "0")
}

fn render_timeout(
    timed_out: &TimedOutProcess,
    timeout_ms: u64,
) -> Result<TimeoutRender, ProcessError> {
    let cancellation = CancellationToken::new();
    project_captures(
        &timed_out.stdout,
        &timed_out.stderr,
        &cancellation,
        |stdout, stderr| timeout_output(timed_out, timeout_ms, stdout, stderr),
        timeout_output_fits_budget,
    )
}

fn timeout_output(
    timed_out: &TimedOutProcess,
    timeout_ms: u64,
    stdout: &RenderedCapture,
    stderr: &RenderedCapture,
) -> TimeoutRender {
    let header = format!(
        "process timed out after {timeout_ms} ms and its process tree was terminated\nResolved program: {}\nLauncher: {}\nCwd: {}\nStatus: timed out; process tree terminated",
        diagnostic_path(&timed_out.resolved.absolute),
        timed_out.resolved.launcher.label(),
        diagnostic_path(&timed_out.cwd)
    );
    let tail = [
        "Exit code: unavailable (timed out)".to_owned(),
        format!("Duration ms: {}", timed_out.duration.as_millis()),
        format!(
            "Stdout bytes: total={}, shown={}, omitted={}, invalid={}",
            timed_out.stdout.bytes_read,
            stdout.shown_bytes,
            stdout.omitted_bytes,
            stdout.invalid_bytes
        ),
        format!(
            "Stderr bytes: total={}, shown={}, omitted={}, invalid={}",
            timed_out.stderr.bytes_read,
            stderr.shown_bytes,
            stderr.omitted_bytes,
            stderr.invalid_bytes
        ),
        "Incomplete.".to_owned(),
    ];
    let mut text = String::with_capacity(
        header
            .len()
            .saturating_add(stdout.text.len())
            .saturating_add(stderr.text.len())
            .saturating_add(256),
    );
    text.push_str(&header);
    text.push_str("\n--- stdout ---\n");
    text.push_str(&stdout.text);
    text.push_str("\n--- stderr ---\n");
    text.push_str(&stderr.text);
    for line in tail {
        text.push('\n');
        text.push_str(&line);
    }
    TimeoutRender {
        text,
        details: ProcessTimeoutDetails {
            timeout_ms,
            program: diagnostic_path(&timed_out.resolved.absolute),
            cwd: diagnostic_path(&timed_out.cwd),
            launcher: timed_out.resolved.launcher.label().to_owned(),
            duration_ms: u64::try_from(timed_out.duration.as_millis()).unwrap_or(u64::MAX),
            stdout: ProcessStreamSummary {
                total: timed_out.stdout.bytes_read,
                shown: stdout.shown_bytes,
                omitted: stdout.omitted_bytes,
                invalid_utf8: stdout.invalid_bytes,
            },
            stderr: ProcessStreamSummary {
                total: timed_out.stderr.bytes_read,
                shown: stderr.shown_bytes,
                omitted: stderr.omitted_bytes,
                invalid_utf8: stderr.invalid_bytes,
            },
            termination_outcome: "terminated",
        },
    }
}

fn timeout_output_fits_budget(output: &TimeoutRender) -> bool {
    serde_json::to_value(&output.details).ok().is_some_and(|details| {
        crate::output::tool_error_result_fits_budget(
            "resource_timeout",
            true,
            &output.text,
            Some(&details),
        )
    })
}

#[derive(Clone, Copy)]
struct CaptureQuotas {
    stdout: usize,
    stderr: usize,
}

fn project_captures<T>(
    stdout: &Capture,
    stderr: &Capture,
    cancellation: &CancellationToken,
    mut build: impl FnMut(&RenderedCapture, &RenderedCapture) -> T,
    mut fits: impl FnMut(&T) -> bool,
) -> Result<T, ProcessError> {
    let maximum = CaptureQuotas {
        stdout: stdout.retained(),
        stderr: stderr.retained(),
    };
    let full = build_capture_candidate(stdout, stderr, maximum, &mut build);
    if fits(&full) {
        return Ok(full);
    }
    check_render_cancellation(cancellation)?;

    let empty = CaptureQuotas {
        stdout: 0,
        stderr: 0,
    };
    let minimal = build_capture_candidate(stdout, stderr, empty, &mut build);
    if !fits(&minimal) {
        return Err(crate::output::OutputError::RequiredContentTooLarge.into());
    }

    let mut low = 0_usize;
    let mut high = maximum.stdout.max(maximum.stderr).saturating_add(1);
    while low + 1 < high {
        check_render_cancellation(cancellation)?;
        let midpoint = low + (high - low) / 2;
        let quotas = CaptureQuotas {
            stdout: midpoint.min(maximum.stdout),
            stderr: midpoint.min(maximum.stderr),
        };
        let candidate = build_capture_candidate(stdout, stderr, quotas, &mut build);
        if fits(&candidate) {
            low = midpoint;
        } else {
            high = midpoint;
        }
    }

    let mut quotas = CaptureQuotas {
        stdout: low.min(maximum.stdout),
        stderr: low.min(maximum.stderr),
    };
    let stdout_remaining = maximum.stdout.saturating_sub(quotas.stdout);
    let stderr_remaining = maximum.stderr.saturating_sub(quotas.stderr);
    let order = if stdout_remaining >= stderr_remaining {
        [true, false]
    } else {
        [false, true]
    };
    for expand_stdout in order {
        let (current, maximum_value) = if expand_stdout {
            (quotas.stdout, maximum.stdout)
        } else {
            (quotas.stderr, maximum.stderr)
        };
        let mut low = current;
        let mut high = maximum_value.saturating_add(1);
        while low + 1 < high {
            check_render_cancellation(cancellation)?;
            let midpoint = low + (high - low) / 2;
            let candidate_quotas = if expand_stdout {
                CaptureQuotas {
                    stdout: midpoint,
                    ..quotas
                }
            } else {
                CaptureQuotas {
                    stderr: midpoint,
                    ..quotas
                }
            };
            let candidate =
                build_capture_candidate(stdout, stderr, candidate_quotas, &mut build);
            if fits(&candidate) {
                low = midpoint;
            } else {
                high = midpoint;
            }
        }
        if expand_stdout {
            quotas.stdout = low;
        } else {
            quotas.stderr = low;
        }
    }

    let candidate = build_capture_candidate(stdout, stderr, quotas, &mut build);
    if fits(&candidate) {
        return Ok(candidate);
    }
    Ok(minimal)
}

fn build_capture_candidate<T>(
    stdout: &Capture,
    stderr: &Capture,
    quotas: CaptureQuotas,
    build: &mut impl FnMut(&RenderedCapture, &RenderedCapture) -> T,
) -> T {
    let stdout = stdout.render(quotas.stdout);
    let stderr = stderr.render(quotas.stderr);
    build(&stdout, &stderr)
}

fn check_render_cancellation(cancellation: &CancellationToken) -> Result<(), ProcessError> {
    if cancellation.is_cancelled() {
        return Err(crate::output::OutputError::Cancelled.into());
    }
    Ok(())
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
