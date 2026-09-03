use std::{path::PathBuf, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{
    path::RepositoryRoot,
    tools::{
        ToolOutput,
        bash::{
            detached::DetachedAdmission,
            locate::{BashLocator, LocateError},
        },
        exec::{
            ProcessError, ProcessStreamSummary, ProcessTimeoutDetails,
            capture::{
                Capture, RenderedCapture, diagnostic_path, project_captures,
                push_capture_diagnostics, push_capture_section, push_output_line,
            },
            resolve::{ResolvedProgram, launcher_for},
            spawn::{
                self, EnvironmentPlan, ExecFailure, ExecPlan, Streams, default_timeout_within,
            },
        },
    },
};

pub mod detached;
pub(crate) mod locate;
pub mod status;
pub use locate::BASH_OVERRIDE_ENV;
#[cfg(test)]
mod tests;

const BASH_MEMORY_BYTES: usize = 2 * 1024 * 1024;
const MSYS_RETRY_HINT: &str =
    "Retry: msys_argument_conversion=\"disabled\" if a native program rejected a /X switch.";
#[cfg(windows)]
const MSYS2_ARG_CONV_EXCL: &str = "MSYS2_ARG_CONV_EXCL";

/// Injected as constants rather than sourced from a profile, so the invariants are testable in
/// Rust instead of living in a shell script the operator can edit.
const BASH_ENVIRONMENT: [(&str, &str); 13] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_PAGER", "cat"),
    ("PAGER", "cat"),
    ("CARGO_TERM_COLOR", "never"),
    ("CLICOLOR", "0"),
    ("FORCE_COLOR", "0"),
    ("EDITOR", "true"),
    ("VISUAL", "true"),
    ("GIT_EDITOR", "true"),
    ("PYTHONUNBUFFERED", "1"),
    ("PYTHONIOENCODING", "utf-8"),
];

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MsysArgumentConversion {
    #[default]
    Default,
    Disabled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BashRequest {
    pub command: String,
    pub cwd: Option<String>,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub detach: bool,
    pub log_path: Option<String>,
    #[serde(default)]
    pub msys_argument_conversion: MsysArgumentConversion,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BashTerminateRequest {
    pub action: BashControlAction,
    pub job_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BashControlAction {
    Terminate,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum BashToolRequest {
    Terminate(BashTerminateRequest),
    Run(BashRequest),
}

impl BashTerminateRequest {
    pub fn validate(&self) -> Result<(), ProcessError> {
        status::validate_job_id(&self.job_id)
    }
}

impl BashRequest {
    /// Validate scalar and combination constraints before foreground admission.
    ///
    /// # Errors
    ///
    /// Returns a validation error for an empty command, an out-of-range timeout, or a
    /// invalid detached log-path combination.
    pub fn validate(&self, timeout_ceiling_ms: u64) -> Result<(), ProcessError> {
        if self.command.is_empty() {
            return Err(invalid("command must not be empty"));
        }
        if self.command.contains('\0') || self.cwd.as_deref().is_some_and(|cwd| cwd.contains('\0'))
        {
            return Err(invalid("command and cwd must not contain NUL"));
        }
        if self.detach {
            if self.log_path.is_none() {
                return Err(invalid("detach requires a log_path inside the repository"));
            }
            if self
                .log_path
                .as_deref()
                .is_some_and(|path| path.contains('\0'))
            {
                return Err(invalid("log_path must not contain NUL"));
            }
            if !(1..=timeout_ceiling_ms).contains(&self.background_timeout_ms(timeout_ceiling_ms)) {
                return Err(invalid(format!(
                    "timeout_ms must be from 1 to {timeout_ceiling_ms} for a detached command"
                )));
            }
            return Ok(());
        }
        if self.log_path.is_some() {
            return Err(invalid("log_path is only accepted when detach is true"));
        }
        if !(1..=timeout_ceiling_ms).contains(&self.timeout_ms(timeout_ceiling_ms)) {
            return Err(invalid(format!(
                "timeout_ms must be from 1 to {timeout_ceiling_ms}"
            )));
        }
        Ok(())
    }

    #[must_use]
    pub fn timeout_ms(&self, timeout_ceiling_ms: u64) -> u64 {
        self.timeout_ms
            .unwrap_or(default_timeout_within(timeout_ceiling_ms))
    }

    #[must_use]
    pub fn background_timeout_ms(&self, background_timeout_max_ms: u64) -> u64 {
        self.timeout_ms.unwrap_or(background_timeout_max_ms)
    }

    #[must_use]
    pub fn memory_charge(&self) -> usize {
        BASH_MEMORY_BYTES
            .saturating_add(self.command.len())
            .saturating_add(self.cwd.as_deref().map_or(0, str::len))
            .saturating_add(self.log_path.as_deref().map_or(0, str::len))
    }
}

pub(in crate::tools::bash) fn invalid(message: impl Into<String>) -> ProcessError {
    ProcessError::Validation(message.into())
}

/// Every command goes through the same non-interactive, profile-free invocation, which is
/// what the tool description promises the caller.
pub fn bash_args(command: &str) -> Vec<String> {
    vec![
        "--noprofile".to_owned(),
        "--norc".to_owned(),
        "-c".to_owned(),
        command.to_owned(),
    ]
}

/// Removed from the inherited environment on every bash launch, foreground and detached
/// alike: non-interactive bash still sources `BASH_ENV` even under `--noprofile --norc`,
/// and `ENV` is the POSIX-mode twin of the same injection path.
pub const STRIPPED_INHERITED_ENV: [&str; 2] = ["BASH_ENV", "ENV"];

pub fn bash_environment(
    runtime: &locate::BashRuntime,
    msys_argument_conversion: MsysArgumentConversion,
) -> EnvironmentPlan {
    let mut plan = EnvironmentPlan::from_defaults(&BASH_ENVIRONMENT);
    plan.removed
        .extend(STRIPPED_INHERITED_ENV.map(str::to_owned));
    plan.injected
        .push(("LANG".to_owned(), runtime.locale.clone()));
    plan.injected
        .push(("LC_ALL".to_owned(), runtime.locale.clone()));
    // An override rather than an injection: this must replace the inherited `PATH`, not sit
    // beside it under a differently cased key.
    if let Some(path) = &runtime.path {
        plan.overrides.push(("PATH".to_owned(), path.clone()));
    }
    #[cfg(windows)]
    if msys_argument_conversion == MsysArgumentConversion::Disabled {
        plan.overrides
            .push((MSYS2_ARG_CONV_EXCL.to_owned(), "*".to_owned()));
    }
    #[cfg(not(windows))]
    let _ = msys_argument_conversion;
    plan
}

/// Resolve the probed bash and run one command line through it.
///
/// # Errors
///
/// Returns validation, bash-discovery, spawn, I/O, cancellation, timeout, cleanup, or output
/// errors.
#[cfg(test)]
pub fn execute_output(
    root: &Arc<RepositoryRoot>,
    locator: &BashLocator,
    detached_admission: Option<DetachedAdmission>,
    request: &BashRequest,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ProcessError> {
    execute_output_with_budget(
        root,
        locator,
        detached_admission,
        request,
        timeout,
        cancellation,
        &crate::output::TestCallBudget::default(),
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "foreground and detached Bash share one validated launch plan"
)]
#[cfg(test)]
pub fn execute_output_with_budget(
    root: &Arc<RepositoryRoot>,
    locator: &BashLocator,
    detached_admission: Option<DetachedAdmission>,
    request: &BashRequest,
    timeout: Duration,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
) -> Result<ToolOutput, ProcessError> {
    execute_output_with_capture(
        root,
        locator,
        detached_admission,
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
    reason = "foreground and detached Bash share one validated launch plan"
)]
pub(crate) fn execute_output_with_capture(
    root: &Arc<RepositoryRoot>,
    locator: &BashLocator,
    detached_admission: Option<DetachedAdmission>,
    request: &BashRequest,
    timeout: Duration,
    cancellation: &CancellationToken,
    timeout_ceiling_ms: u64,
    output_budget: &dyn crate::output::CallBudget,
    capture_sink: Option<&Arc<dyn spawn::CaptureSink>>,
) -> Result<ToolOutput, ProcessError> {
    if !request.detach {
        let prepared = prepare_bash_foreground(
            root,
            locator,
            request,
            timeout,
            timeout_ceiling_ms,
            cancellation,
        )?;
        return execute_prepared_bash(prepared, None, cancellation, output_budget, capture_sink);
    }
    detached::execute_detached(
        root,
        locator,
        detached_admission,
        request,
        timeout,
        cancellation,
        timeout_ceiling_ms,
        output_budget,
    )
}

/// One resolved foreground bash launch: the probed bash executable, its `-c`
/// argv, working directory, locale environment, and timeout.
#[derive(Debug)]
pub(crate) struct PreparedBash {
    pub(crate) resolved: ResolvedProgram,
    pub(crate) cwd: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) environment: EnvironmentPlan,
    pub(crate) deadline: std::time::Instant,
    pub(crate) request_timeout_ms: u64,
    pub(crate) msys_retry_available: bool,
}

/// Resolve the probed bash and validate one foreground command launch without
/// spawning it, so a host can wrap the final argv in a sandbox first.
pub(crate) fn prepare_bash_foreground(
    root: &Arc<RepositoryRoot>,
    locator: &BashLocator,
    request: &BashRequest,
    timeout: Duration,
    timeout_ceiling_ms: u64,
    cancellation: &CancellationToken,
) -> Result<PreparedBash, ProcessError> {
    let deadline = std::time::Instant::now() + timeout;
    request.validate(timeout_ceiling_ms)?;
    let runtime = locator
        .resolve_before(cancellation, deadline)
        .map_err(|error| match error {
            LocateError::Cancelled => ProcessError::Cancelled,
            LocateError::TimedOut => ProcessError::TimeoutBeforeSpawn {
                timeout_ms: request.timeout_ms(timeout_ceiling_ms),
            },
            LocateError::Unavailable(message) => ProcessError::Unavailable(message.to_string()),
        })?;
    tracing::info!(target: "agentshim", event = "process_resolve", phase = "execution");
    let cwd = spawn::resolve_cwd(root, request.cwd.as_deref()).map_err(invalid)?;
    let resolved = ResolvedProgram {
        absolute: runtime.executable.clone(),
        executable: runtime.executable.clone(),
        launcher: launcher_for(&runtime.executable)?,
    };
    Ok(PreparedBash {
        resolved,
        cwd,
        args: bash_args(&request.command),
        environment: bash_environment(&runtime, request.msys_argument_conversion),
        deadline,
        request_timeout_ms: request.timeout_ms(timeout_ceiling_ms),
        msys_retry_available: msys_retry_available(request),
    })
}

/// Spawn one prepared foreground bash launch, merged streams, with the same
/// tree ownership, timeout, capture ceiling, and cancellation as other hosts.
pub(crate) fn execute_prepared_bash(
    prepared: PreparedBash,
    wrapped_argv: Option<&[String]>,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
    capture_sink: Option<&Arc<dyn spawn::CaptureSink>>,
) -> Result<ToolOutput, ProcessError> {
    let PreparedBash {
        mut resolved,
        cwd,
        args,
        environment,
        deadline,
        request_timeout_ms,
        msys_retry_available,
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
    if let Some(argv) = wrapped_argv {
        let command = argv
            .first()
            .ok_or_else(|| invalid("wrapped argv must contain at least the executable"))?;
        resolved = ResolvedProgram {
            absolute: std::path::PathBuf::from(command),
            executable: std::path::PathBuf::from(command),
            launcher: launcher_for(std::path::Path::new(command))?,
        };
    }
    let args = wrapped_argv.map_or(args, |argv| argv[1.min(argv.len())..].to_vec());
    let plan = ExecPlan {
        resolved: &resolved,
        cwd: &cwd,
        args: &args,
        environment: &environment,
        stdin: None,
        streams: Streams::Merged,
        timeout,
        capture_page_bytes: output_budget.page_bytes(),
    };
    match spawn::run_with_capture(&plan, cancellation, capture_sink) {
        Ok(outcome) => render_completed_with_budget(
            &CompletedBash {
                bash: resolved.absolute,
                cwd,
                exit: outcome.exit,
                duration: outcome.duration,
                output: expect_one(outcome.captures),
                msys_retry_available,
            },
            cancellation,
            output_budget,
        ),
        Err(ExecFailure::TimedOut { duration, captures }) => {
            let report = render_timeout(
                &TimedOutBash {
                    bash: resolved.absolute,
                    cwd,
                    duration,
                    output: expect_one(captures),
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

fn expect_one(captures: Vec<Capture>) -> Capture {
    let count = captures.len();
    captures
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("a merged topology always yields one capture, got {count}"))
}

struct CompletedBash {
    bash: PathBuf,
    cwd: PathBuf,
    exit: String,
    duration: Duration,
    output: Capture,
    msys_retry_available: bool,
}

struct TimedOutBash {
    bash: PathBuf,
    cwd: PathBuf,
    duration: Duration,
    output: Capture,
}

struct TimeoutRender {
    text: String,
    details: ProcessTimeoutDetails,
}

#[cfg(test)]
fn render_completed(
    completed: &CompletedBash,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ProcessError> {
    render_completed_with_budget(
        completed,
        cancellation,
        &crate::output::TestCallBudget::default(),
    )
}

fn render_completed_with_budget(
    completed: &CompletedBash,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
) -> Result<ToolOutput, ProcessError> {
    project_captures(
        &[&completed.output],
        cancellation,
        |rendered| completed_output(completed, &rendered[0]),
        |output| output.fits_content_and_call(output_budget, cancellation),
    )
    .map_err(|error| normalize_burst_render_error(error, output_budget))
}

/// Offered from the command text rather than from the child's diagnostics: every native
/// program words an unknown-switch failure differently, but the syntax that provokes Git
/// Bash into rewriting the argument is fixed. Deliberately conservative — a missed hint
/// costs nothing because the parameter is still documented, while a hint on every failing
/// `ls /tmp` would be noise on the most common commands.
fn msys_retry_available(request: &BashRequest) -> bool {
    cfg!(windows)
        && matches!(
            request.msys_argument_conversion,
            MsysArgumentConversion::Default
        )
        && request.command.split_whitespace().any(is_slash_switch)
}

/// `/E` and `/MIR` are switches; `/tmp` and `/usr/bin` are POSIX paths. Requiring short,
/// uppercase bodies keeps single-segment absolute paths out.
fn is_slash_switch(token: &str) -> bool {
    let Some(switch) = token.strip_prefix('/') else {
        return false;
    };
    !switch.is_empty()
        && switch.len() <= 3
        && switch
            .chars()
            .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
        && switch
            .chars()
            .any(|character| character.is_ascii_uppercase())
}

fn completed_output(completed: &CompletedBash, output: &RenderedCapture) -> ToolOutput {
    let child_nonzero = completed.exit != "0";
    let mut rendered = String::with_capacity(output.text.len().saturating_add(192));
    if child_nonzero {
        push_output_line(
            &mut rendered,
            &format!("Bash: {}", diagnostic_path(&completed.bash)),
        );
        push_output_line(
            &mut rendered,
            &format!("Cwd: {}", diagnostic_path(&completed.cwd)),
        );
    }
    push_capture_section(&mut rendered, "output", output);
    push_output_line(&mut rendered, &format!("Exit code: {}", completed.exit));
    if child_nonzero {
        push_output_line(
            &mut rendered,
            &format!("Duration ms: {}", completed.duration.as_millis()),
        );
        if completed.msys_retry_available {
            push_output_line(&mut rendered, MSYS_RETRY_HINT);
        }
    }
    push_capture_diagnostics(&mut rendered, "Output", completed.output.bytes_read, output);
    ToolOutput::with_child_nonzero(rendered.clone(), child_nonzero).with_structured(json!({
        "tool": "bash",
        "process": {
            "exitCode": completed.exit,
            "stdout": {
                "text": output.text,
                "totalBytes": completed.output.bytes_read,
                "shownBytes": output.shown_bytes,
                "omittedBytes": output.omitted_bytes,
            },
            "stderr": {
                "text": output.text,
                "totalBytes": completed.output.bytes_read,
                "shownBytes": output.shown_bytes,
                "omittedBytes": output.omitted_bytes,
            }
        }
    }))
}

fn render_timeout(
    timed_out: &TimedOutBash,
    timeout_ms: u64,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
) -> Result<TimeoutRender, ProcessError> {
    project_captures(
        &[&timed_out.output],
        cancellation,
        |rendered| timeout_output(timed_out, timeout_ms, &rendered[0]),
        |render: &TimeoutRender| {
            serde_json::to_value(&render.details)
                .ok()
                .is_some_and(|details| {
                    let structured = crate::output::tool_error_structure(
                        "resource_timeout",
                        true,
                        &render.text,
                        Some(&details),
                    );
                    crate::output::tool_result_encoded_len(&render.text, Some(&structured), true)
                        <= crate::output::OutputLimits::for_content_within(
                            &render.text,
                            output_budget.page_bytes(),
                        )
                        .bytes
                        && output_budget.token_gate().is_none_or(|token_gate| {
                            matches!(
                                token_gate.project_result(
                                    &render.text,
                                    Some(&structured),
                                    true,
                                    cancellation
                                ),
                                crate::output::ProjectionDecision::Fits(_)
                            )
                        })
                })
        },
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
    timed_out: &TimedOutBash,
    timeout_ms: u64,
    output: &RenderedCapture,
) -> TimeoutRender {
    let header = format!(
        "bash timed out after {timeout_ms} ms and its owned process containment was terminated\nBash: {}\nCwd: {}\nStatus: timed out; owned process containment terminated",
        diagnostic_path(&timed_out.bash),
        diagnostic_path(&timed_out.cwd)
    );
    let tail = [
        "Exit code: unavailable (timed out)".to_owned(),
        format!("Duration ms: {}", timed_out.duration.as_millis()),
        format!(
            "Output bytes: total={}, shown={}, omitted={}, invalid={}, encoding={}",
            timed_out.output.bytes_read,
            output.shown_bytes,
            output.omitted_bytes,
            output.invalid_bytes,
            output.encoding
        ),
        "Incomplete.".to_owned(),
    ];
    let mut text = String::with_capacity(
        header
            .len()
            .saturating_add(output.text.len())
            .saturating_add(192),
    );
    text.push_str(&header);
    text.push_str("\n--- output ---\n");
    text.push_str(&output.text);
    for line in tail {
        text.push('\n');
        text.push_str(&line);
    }
    let summary = ProcessStreamSummary {
        total: timed_out.output.bytes_read,
        shown: output.shown_bytes,
        omitted: output.omitted_bytes,
        invalid_utf8: output.invalid_bytes,
        encoding: output.encoding.clone(),
    };
    TimeoutRender {
        text,
        details: ProcessTimeoutDetails {
            timeout_ms,
            program: diagnostic_path(&timed_out.bash),
            cwd: diagnostic_path(&timed_out.cwd),
            launcher: "native".to_owned(),
            duration_ms: u64::try_from(timed_out.duration.as_millis()).unwrap_or(u64::MAX),
            stdout: summary.clone(),
            stderr: summary,
            termination_outcome: "terminated",
            containment_scope: crate::tools::exec::containment_scope(),
        },
    }
}
