use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{
    path::RepositoryRoot,
    tools::{
        ToolOutput,
        exec::{
            ProcessError, ProcessStreamSummary, ProcessTimeoutDetails,
            capture::{
                Capture, RenderedCapture, diagnostic_path, project_captures,
                push_capture_diagnostics, push_capture_section, push_output_line,
            },
            resolve::{Launcher, ProcessResolver, ResolvedProgram, launcher_for},
            spawn::{
                self, EnvironmentPlan, ExecFailure, ExecPlan, Streams, default_timeout_within,
            },
        },
    },
};

#[cfg(test)]
mod tests;

const MAX_STDIN_BYTES: usize = 1024 * 1024;
const PROCESS_MEMORY_BYTES: usize = 2 * 1024 * 1024;

pub const ENVIRONMENT_DEFAULTS: [(&str, &str); 6] = [
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
    pub fn validate(&self, timeout_ceiling_ms: u64) -> Result<(), ProcessError> {
        if self.program.is_empty() {
            return Err(invalid("program must not be empty"));
        }
        if contains_nul(&self.program)
            || self.args.iter().any(|arg| contains_nul(arg))
            || self.cwd.as_deref().is_some_and(contains_nul)
            || self.stdin.as_deref().is_some_and(contains_nul)
        {
            return Err(invalid(
                "program, args, cwd, and stdin must not contain NUL",
            ));
        }
        if self
            .stdin
            .as_ref()
            .is_some_and(|stdin| stdin.len() > MAX_STDIN_BYTES)
        {
            return Err(invalid("stdin must not exceed 1048576 UTF-8 bytes"));
        }
        if !(1..=timeout_ceiling_ms).contains(&self.timeout_ms(timeout_ceiling_ms)) {
            return Err(invalid(format!(
                "timeout_ms must be from 1 to {timeout_ceiling_ms}"
            )));
        }

        let mut overrides: Vec<String> = Vec::new();
        for (key, value) in &self.env {
            validate_environment(key, value)?;
            if overrides
                .iter()
                .any(|existing| environment_keys_equal(existing, key))
            {
                return Err(invalid(format!(
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
                return Err(invalid(format!("unset_env contains duplicate key {key:?}")));
            }
            if overrides
                .iter()
                .any(|existing| environment_keys_equal(existing, key))
            {
                return Err(invalid(format!(
                    "environment key {key:?} occurs in both env and unset_env"
                )));
            }
            removals.push(key.clone());
        }
        Ok(())
    }

    #[must_use]
    pub fn timeout_ms(&self, timeout_ceiling_ms: u64) -> u64 {
        self.timeout_ms
            .unwrap_or(default_timeout_within(timeout_ceiling_ms))
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
        PROCESS_MEMORY_BYTES
            .saturating_add(self.program.len())
            .saturating_add(self.cwd.as_deref().map_or(0, str::len))
            .saturating_add(self.stdin.as_deref().map_or(0, str::len))
            .saturating_add(strings)
    }

    fn environment(&self) -> EnvironmentPlan {
        let mut plan = EnvironmentPlan::from_defaults(&ENVIRONMENT_DEFAULTS);
        plan.removed.clone_from(&self.unset_env);
        plan.overrides = self
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        plan
    }
}

fn invalid(message: impl Into<String>) -> ProcessError {
    ProcessError::Validation(message.into())
}

fn contains_nul(value: &str) -> bool {
    value.contains('\0')
}

fn validate_environment(key: &str, value: &str) -> Result<(), ProcessError> {
    if key.is_empty() || key.contains('=') || contains_nul(key) || contains_nul(value) {
        return Err(invalid(format!(
            "invalid environment key or value for {key:?}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_base_environment(entries: &[(String, String)]) -> Result<(), ProcessError> {
    let mut seen: Vec<&str> = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        validate_environment(key, value)?;
        if seen
            .iter()
            .any(|existing| environment_keys_equal(existing, key))
        {
            return Err(invalid(format!(
                "base environment contains duplicate key {key:?} under platform comparison rules"
            )));
        }
        seen.push(key);
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

/// Resolve and execute one structured process request.
///
/// # Errors
///
/// Returns validation, resolution, spawn, I/O, cancellation, timeout, cleanup, or output errors.
#[cfg(any(test, feature = "bench-internals"))]
pub fn execute(
    root: &Arc<RepositoryRoot>,
    resolver: &ProcessResolver,
    request: &ProcessRequest,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<String, ProcessError> {
    execute_output(root, resolver, request, timeout, cancellation).map(|result| result.text)
}

#[cfg(any(test, feature = "bench-internals"))]
pub fn execute_output(
    root: &Arc<RepositoryRoot>,
    resolver: &ProcessResolver,
    request: &ProcessRequest,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ProcessError> {
    execute_output_with_budget(
        root,
        resolver,
        request,
        timeout,
        cancellation,
        &crate::output::TestCallBudget::default(),
    )
}

#[cfg(any(test, feature = "bench-internals"))]
pub fn execute_output_with_budget(
    root: &Arc<RepositoryRoot>,
    resolver: &ProcessResolver,
    request: &ProcessRequest,
    timeout: Duration,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
) -> Result<ToolOutput, ProcessError> {
    execute_output_with_capture(
        root,
        resolver,
        request,
        timeout,
        cancellation,
        crate::tools::exec::spawn::default_max_timeout_ms(),
        output_budget,
        None,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the execution entry mirrors the bash entry: bounds, budget, and optional capture"
)]
/// One resolved foreground `run_program` launch: final executable, argv, working
/// directory, environment plan, and timeout, ready to spawn directly or through a
/// sandbox-wrapped argv.
#[derive(Debug)]
pub(crate) struct PreparedRunProgram {
    pub(crate) resolved: ResolvedProgram,
    pub(crate) cwd: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) environment: EnvironmentPlan,
    pub(crate) stdin: Option<String>,
    pub(crate) deadline: std::time::Instant,
    pub(crate) request_timeout_ms: u64,
}

pub(crate) fn prepare_run_program(
    root: &Arc<RepositoryRoot>,
    resolver: &ProcessResolver,
    request: &ProcessRequest,
    timeout: Duration,
    timeout_ceiling_ms: u64,
) -> Result<PreparedRunProgram, ProcessError> {
    let started = std::time::Instant::now();
    let deadline = started + timeout;
    request.validate(timeout_ceiling_ms)?;
    ensure_before_spawn(deadline, request.timeout_ms(timeout_ceiling_ms))?;
    tracing::info!(target: "agentshim", event = "process_resolve", phase = "execution");
    let cwd = spawn::resolve_cwd(root, request.cwd.as_deref()).map_err(invalid)?;
    ensure_before_spawn(deadline, request.timeout_ms(timeout_ceiling_ms))?;
    let program = resolver.resolve(&request.program, &cwd)?;
    ensure_before_spawn(deadline, request.timeout_ms(timeout_ceiling_ms))?;
    Ok(PreparedRunProgram {
        resolved: program,
        cwd,
        args: request.args.clone(),
        environment: request.environment(),
        stdin: request.stdin.clone(),
        deadline,
        request_timeout_ms: request.timeout_ms(timeout_ceiling_ms),
    })
}

/// Spawn one prepared foreground launch. `wrapped_argv` replaces the resolved argv
/// wholesale when a sandbox wrapped it; the engine keeps tree ownership, pipes,
/// timeout, capture ceiling, and cancellation either way.
pub(crate) fn execute_prepared_run_program(
    prepared: PreparedRunProgram,
    wrapped_argv: Option<&[String]>,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
    capture_sink: Option<&Arc<dyn spawn::CaptureSink>>,
) -> Result<ToolOutput, ProcessError> {
    let PreparedRunProgram {
        resolved,
        cwd,
        args,
        environment,
        stdin,
        deadline,
        request_timeout_ms,
    } = prepared;
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    let timeout = deadline
        .checked_duration_since(std::time::Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(ProcessError::TimeoutBeforeSpawn {
            timeout_ms: request_timeout_ms,
        })?;
    let mut resolved = resolved;
    if let Some(argv) = wrapped_argv {
        let command = argv
            .first()
            .ok_or_else(|| invalid("wrapped argv must contain at least the executable"))?;
        resolved = ResolvedProgram {
            absolute: PathBuf::from(command),
            executable: PathBuf::from(command),
            launcher: launcher_for(std::path::Path::new(command))?,
        };
    }
    let args = wrapped_argv.map_or(args, |argv| argv[1.min(argv.len())..].to_vec());
    let plan = ExecPlan {
        resolved: &resolved,
        cwd: &cwd,
        args: &args,
        environment: &environment,
        stdin: stdin.as_deref(),
        streams: Streams::Separate,
        timeout,
        capture_page_bytes: output_budget.page_bytes(),
    };
    match spawn::run_with_capture(&plan, cancellation, capture_sink) {
        Ok(outcome) => {
            let [stdout, stderr] = expect_two(outcome.captures);
            render_completed_with_budget(
                &CompletedProcess {
                    resolved,
                    cwd,
                    exit: outcome.exit,
                    duration: outcome.duration,
                    stdout,
                    stderr,
                },
                cancellation,
                output_budget,
            )
        }
        Err(ExecFailure::TimedOut { duration, captures }) => {
            let [stdout, stderr] = expect_two(captures);
            let report = render_timeout_with_budget(
                &TimedOutProcess {
                    resolved,
                    cwd,
                    duration,
                    stdout,
                    stderr,
                },
                request_timeout_ms,
                cancellation,
                output_budget,
            )?;
            Err(ProcessError::Timeout {
                timeout_ms: request_timeout_ms,
                report: report.text,
                details: Box::new(report.details),
            })
        }
        Err(failure) => Err(failure.into_process_error(request_timeout_ms)),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the execution entry mirrors the bash entry: bounds, budget, and optional capture"
)]
#[cfg(any(test, feature = "bench-internals"))]
pub(crate) fn execute_output_with_capture(
    root: &Arc<RepositoryRoot>,
    resolver: &ProcessResolver,
    request: &ProcessRequest,
    timeout: Duration,
    cancellation: &CancellationToken,
    timeout_ceiling_ms: u64,
    output_budget: &dyn crate::output::CallBudget,
    capture_sink: Option<&Arc<dyn spawn::CaptureSink>>,
) -> Result<ToolOutput, ProcessError> {
    let prepared = prepare_run_program(root, resolver, request, timeout, timeout_ceiling_ms)?;
    execute_prepared_run_program(prepared, None, cancellation, output_budget, capture_sink)
}

fn ensure_before_spawn(deadline: std::time::Instant, timeout_ms: u64) -> Result<(), ProcessError> {
    if std::time::Instant::now() >= deadline {
        Err(ProcessError::TimeoutBeforeSpawn { timeout_ms })
    } else {
        Ok(())
    }
}

fn expect_two(captures: Vec<Capture>) -> [Capture; 2] {
    let count = captures.len();
    captures
        .try_into()
        .unwrap_or_else(|_| panic!("a separate topology always yields two captures, got {count}"))
}

pub struct CompletedProcess {
    pub resolved: ResolvedProgram,
    pub cwd: PathBuf,
    pub exit: String,
    pub duration: Duration,
    pub stdout: Capture,
    pub stderr: Capture,
}

pub struct TimedOutProcess {
    pub resolved: ResolvedProgram,
    pub cwd: PathBuf,
    pub duration: Duration,
    pub stdout: Capture,
    pub stderr: Capture,
}

pub struct TimeoutRender {
    pub text: String,
    pub details: ProcessTimeoutDetails,
}

impl std::ops::Deref for TimeoutRender {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

#[cfg(test)]
pub fn render_completed(
    completed: &CompletedProcess,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ProcessError> {
    render_completed_with_budget(
        completed,
        cancellation,
        &crate::output::TestCallBudget::default(),
    )
}

fn render_completed_with_budget(
    completed: &CompletedProcess,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
) -> Result<ToolOutput, ProcessError> {
    project_captures(
        &[&completed.stdout, &completed.stderr],
        cancellation,
        |rendered| completed_output(completed, &rendered[0], &rendered[1]),
        |output| output.fits_content_and_call(output_budget, cancellation),
    )
}

fn completed_output(
    completed: &CompletedProcess,
    stdout: &RenderedCapture,
    stderr: &RenderedCapture,
) -> ToolOutput {
    let child_nonzero = completed.exit != "0";
    let mut rendered = String::with_capacity(
        stdout
            .text
            .len()
            .saturating_add(stderr.text.len())
            .saturating_add(256),
    );
    if child_nonzero || completed.resolved.launcher != Launcher::Native {
        push_output_line(
            &mut rendered,
            &format!(
                "Resolved program: {}",
                diagnostic_path(&completed.resolved.absolute)
            ),
        );
        push_output_line(
            &mut rendered,
            &format!("Launcher: {}", spawn::launcher_label(&completed.resolved)),
        );
    }
    if child_nonzero {
        push_output_line(
            &mut rendered,
            &format!("Cwd: {}", diagnostic_path(&completed.cwd)),
        );
    }
    push_capture_section(&mut rendered, "stdout", stdout);
    push_capture_section(&mut rendered, "stderr", stderr);
    push_output_line(&mut rendered, &format!("Exit code: {}", completed.exit));
    if child_nonzero {
        push_output_line(
            &mut rendered,
            &format!("Duration ms: {}", completed.duration.as_millis()),
        );
    }
    push_capture_diagnostics(&mut rendered, "Stdout", completed.stdout.bytes_read, stdout);
    push_capture_diagnostics(&mut rendered, "Stderr", completed.stderr.bytes_read, stderr);
    ToolOutput::with_child_nonzero(rendered.clone(), child_nonzero).with_structured(json!({
        "tool": "run_program",
        "process": {
            "exitCode": completed.exit,
            "stdout": {
                "text": stdout.text,
                "totalBytes": completed.stdout.bytes_read,
                "shownBytes": stdout.shown_bytes,
                "omittedBytes": stdout.omitted_bytes,
            },
            "stderr": {
                "text": stderr.text,
                "totalBytes": completed.stderr.bytes_read,
                "shownBytes": stderr.shown_bytes,
                "omittedBytes": stderr.omitted_bytes,
            }
        }
    }))
}

#[cfg(test)]
pub fn render_timeout(
    timed_out: &TimedOutProcess,
    timeout_ms: u64,
    cancellation: &CancellationToken,
) -> Result<TimeoutRender, ProcessError> {
    render_timeout_with_budget(
        timed_out,
        timeout_ms,
        cancellation,
        &crate::output::TestCallBudget::default(),
    )
}

fn render_timeout_with_budget(
    timed_out: &TimedOutProcess,
    timeout_ms: u64,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
) -> Result<TimeoutRender, ProcessError> {
    project_captures(
        &[&timed_out.stdout, &timed_out.stderr],
        cancellation,
        |rendered| timeout_output(timed_out, timeout_ms, &rendered[0], &rendered[1]),
        |output| timeout_output_fits_budget(output, cancellation, output_budget),
    )
    .map_err(|error| normalize_burst_render_error(error, output_budget))
}

fn normalize_burst_render_error(
    error: ProcessError,
    output_budget: &dyn crate::output::CallBudget,
) -> ProcessError {
    if output_budget
        .token_gate()
        .is_some_and(|token_gate| token_gate.ceiling() < crate::output::CALL_OUTPUT_TOKEN_LIMIT)
        && matches!(
            error,
            ProcessError::Output(crate::output::OutputError::RequiredContentTooLarge)
        )
    {
        ProcessError::Output(crate::output::OutputError::BurstLimit)
    } else {
        error
    }
}

fn timeout_output(
    timed_out: &TimedOutProcess,
    timeout_ms: u64,
    stdout: &RenderedCapture,
    stderr: &RenderedCapture,
) -> TimeoutRender {
    let header = format!(
        "process timed out after {timeout_ms} ms and its owned process containment was terminated\nResolved program: {}\nLauncher: {}\nCwd: {}\nStatus: timed out; owned process containment terminated",
        diagnostic_path(&timed_out.resolved.absolute),
        spawn::launcher_label(&timed_out.resolved),
        diagnostic_path(&timed_out.cwd)
    );
    let tail = [
        "Exit code: unavailable (timed out)".to_owned(),
        format!("Duration ms: {}", timed_out.duration.as_millis()),
        format!(
            "Stdout bytes: total={}, shown={}, omitted={}, invalid={}, encoding={}",
            timed_out.stdout.bytes_read,
            stdout.shown_bytes,
            stdout.omitted_bytes,
            stdout.invalid_bytes,
            stdout.encoding
        ),
        format!(
            "Stderr bytes: total={}, shown={}, omitted={}, invalid={}, encoding={}",
            timed_out.stderr.bytes_read,
            stderr.shown_bytes,
            stderr.omitted_bytes,
            stderr.invalid_bytes,
            stderr.encoding
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
            launcher: spawn::launcher_label(&timed_out.resolved).to_owned(),
            duration_ms: u64::try_from(timed_out.duration.as_millis()).unwrap_or(u64::MAX),
            stdout: ProcessStreamSummary {
                total: timed_out.stdout.bytes_read,
                shown: stdout.shown_bytes,
                omitted: stdout.omitted_bytes,
                invalid_utf8: stdout.invalid_bytes,
                encoding: stdout.encoding.clone(),
            },
            stderr: ProcessStreamSummary {
                total: timed_out.stderr.bytes_read,
                shown: stderr.shown_bytes,
                omitted: stderr.omitted_bytes,
                invalid_utf8: stderr.invalid_bytes,
                encoding: stderr.encoding.clone(),
            },
            termination_outcome: "terminated",
            containment_scope: crate::tools::exec::containment_scope(),
        },
    }
}

fn timeout_output_fits_budget(
    output: &TimeoutRender,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
) -> bool {
    serde_json::to_value(&output.details)
        .ok()
        .is_some_and(|details| {
            let structured = crate::output::tool_error_structure(
                "resource_timeout",
                true,
                &output.text,
                Some(&details),
            );
            crate::output::tool_result_encoded_len(&output.text, Some(&structured), true)
                <= crate::output::OutputLimits::for_content_within(
                    &output.text,
                    output_budget.page_bytes(),
                )
                .bytes
                && output_budget.token_gate().is_none_or(|token_gate| {
                    matches!(
                        token_gate.project_result(
                            &output.text,
                            Some(&structured),
                            true,
                            cancellation
                        ),
                        crate::output::ProjectionDecision::Fits(_)
                    )
                })
        })
}
