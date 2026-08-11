use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    path::RepositoryRoot,
    tools::{
        ToolOutput,
        exec::{
            ProcessError, ProcessResolver, ProcessStreamSummary, ProcessTimeoutDetails,
            capture::{Capture, RenderedCapture, diagnostic_path, project_captures},
            resolve::ResolvedProgram,
            spawn::{
                self, DEFAULT_TIMEOUT_MS, EnvironmentPlan, ExecFailure, ExecPlan, MAX_TIMEOUT_MS,
                Streams,
            },
        },
    },
};

#[cfg(test)]
mod tests;

const MAX_STDIN_BYTES: usize = 1024 * 1024;
const PROCESS_MEMORY_BYTES: usize = 2 * 1024 * 1024;

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
        if !(1..=MAX_TIMEOUT_MS).contains(&self.timeout_ms()) {
            return Err(invalid(format!(
                "timeout_ms must be from 1 to {MAX_TIMEOUT_MS}"
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
    let started = std::time::Instant::now();
    let deadline = started + timeout;
    request.validate()?;
    ensure_before_spawn(deadline, request.timeout_ms())?;
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    tracing::info!(target: "codexshim", event = "process_resolve", phase = "execution");
    let cwd = spawn::resolve_cwd(root, request.cwd.as_deref()).map_err(invalid)?;
    ensure_before_spawn(deadline, request.timeout_ms())?;
    let program = resolver.resolve(&request.program, &cwd)?;
    ensure_before_spawn(deadline, request.timeout_ms())?;
    let timeout = deadline
        .checked_duration_since(std::time::Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(ProcessError::TimeoutBeforeSpawn {
            timeout_ms: request.timeout_ms(),
        })?;
    let environment = request.environment();
    let plan = ExecPlan {
        resolved: &program,
        cwd: &cwd,
        args: &request.args,
        environment: &environment,
        stdin: request.stdin.as_deref(),
        streams: Streams::Separate,
        timeout,
    };
    match spawn::run(&plan, cancellation) {
        Ok(outcome) => {
            let [stdout, stderr] = expect_two(outcome.captures);
            render_completed(
                &CompletedProcess {
                    resolved: program,
                    cwd,
                    exit: outcome.exit,
                    duration: outcome.duration,
                    stdout,
                    stderr,
                },
                cancellation,
            )
        }
        Err(ExecFailure::TimedOut { duration, captures }) => {
            let [stdout, stderr] = expect_two(captures);
            let timeout_ms = request.timeout_ms();
            let report = render_timeout(
                &TimedOutProcess {
                    resolved: program,
                    cwd,
                    duration,
                    stdout,
                    stderr,
                },
                timeout_ms,
                cancellation,
            )?;
            Err(ProcessError::Timeout {
                timeout_ms,
                report: report.text,
                details: Box::new(report.details),
            })
        }
        Err(failure) => Err(failure.into_process_error(request.timeout_ms())),
    }
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

pub(crate) struct CompletedProcess {
    pub(crate) resolved: ResolvedProgram,
    pub(crate) cwd: PathBuf,
    pub(crate) exit: String,
    pub(crate) duration: Duration,
    pub(crate) stdout: Capture,
    pub(crate) stderr: Capture,
}

pub(crate) struct TimedOutProcess {
    pub(crate) resolved: ResolvedProgram,
    pub(crate) cwd: PathBuf,
    pub(crate) duration: Duration,
    pub(crate) stdout: Capture,
    pub(crate) stderr: Capture,
}

pub(crate) struct TimeoutRender {
    pub(crate) text: String,
    pub(crate) details: ProcessTimeoutDetails,
}

impl std::ops::Deref for TimeoutRender {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

pub(crate) fn render_completed(
    completed: &CompletedProcess,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ProcessError> {
    project_captures(
        &[&completed.stdout, &completed.stderr],
        cancellation,
        |rendered| completed_output(completed, &rendered[0], &rendered[1]),
        |output| output.fits_content_and_model(cancellation),
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
        spawn::launcher_label(&completed.resolved),
        diagnostic_path(&completed.cwd)
    );
    let tail = [
        format!("Exit code: {}", completed.exit),
        format!("Duration ms: {}", completed.duration.as_millis()),
        format!(
            "Stdout bytes: total={}, shown={}, omitted={}, invalid={}, encoding={}",
            completed.stdout.bytes_read,
            stdout.shown_bytes,
            stdout.omitted_bytes,
            stdout.invalid_bytes,
            stdout.encoding
        ),
        format!(
            "Stderr bytes: total={}, shown={}, omitted={}, invalid={}, encoding={}",
            completed.stderr.bytes_read,
            stderr.shown_bytes,
            stderr.omitted_bytes,
            stderr.invalid_bytes,
            stderr.encoding
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

pub(crate) fn render_timeout(
    timed_out: &TimedOutProcess,
    timeout_ms: u64,
    cancellation: &CancellationToken,
) -> Result<TimeoutRender, ProcessError> {
    project_captures(
        &[&timed_out.stdout, &timed_out.stderr],
        cancellation,
        |rendered| timeout_output(timed_out, timeout_ms, &rendered[0], &rendered[1]),
        |output| timeout_output_fits_budget(output, cancellation),
    )
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

fn timeout_output_fits_budget(output: &TimeoutRender, cancellation: &CancellationToken) -> bool {
    serde_json::to_value(&output.details)
        .ok()
        .is_some_and(|details| {
            crate::output::tool_error_result_fits_content_budget(
                "resource_timeout",
                true,
                &output.text,
                Some(&details),
            ) && {
                let structured = crate::output::tool_error_structure(
                    "resource_timeout",
                    true,
                    &output.text,
                    Some(&details),
                );
                crate::output::structured_result_fits_model_budget(&structured, cancellation)
            }
        })
}
