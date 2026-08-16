use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

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
pub mod locate;
pub mod status;
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
    pub server_capture: bool,
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
    /// Validate scalar and combination constraints before process admission.
    ///
    /// # Errors
    ///
    /// Returns a validation error for an empty command, an out-of-range timeout, or a
    /// `detach`/`log_path`/`timeout_ms` combination the tool does not accept.
    pub fn validate(&self, timeout_ceiling_ms: u64) -> Result<(), ProcessError> {
        if self.command.is_empty() {
            return Err(invalid("command must not be empty"));
        }
        if self.command.contains('\0') || self.cwd.as_deref().is_some_and(|cwd| cwd.contains('\0'))
        {
            return Err(invalid("command and cwd must not contain NUL"));
        }
        if self.detach {
            let capture_targets =
                usize::from(self.server_capture) + usize::from(self.log_path.is_some());
            if capture_targets != 1 {
                return Err(invalid(
                    "detach requires exactly one of log_path or server_capture=true",
                ));
            }
            if self.timeout_ms.is_some() {
                return Err(invalid(
                    "timeout_ms does not apply to a detached command; it runs until it exits or \
                     the server stops",
                ));
            }
            if self
                .log_path
                .as_deref()
                .is_some_and(|path| path.contains('\0'))
            {
                return Err(invalid("log_path must not contain NUL"));
            }
            return Ok(());
        }
        if self.log_path.is_some() {
            return Err(invalid("log_path is only accepted when detach is true"));
        }
        if self.server_capture {
            return Err(invalid(
                "server_capture is only accepted when detach is true",
            ));
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
    pub fn memory_charge(&self) -> usize {
        BASH_MEMORY_BYTES
            .saturating_add(self.command.len())
            .saturating_add(self.cwd.as_deref().map_or(0, str::len))
            .saturating_add(self.log_path.as_deref().map_or(0, str::len))
    }
}

fn invalid(message: impl Into<String>) -> ProcessError {
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
    clippy::too_many_lines,
    reason = "foreground and detached Bash share one validated launch plan"
)]
pub fn execute_output_with_capture(
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
    let started = std::time::Instant::now();
    request.validate(timeout_ceiling_ms)?;
    let deadline = (!request.detach).then(|| started + timeout);
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    let runtime = match deadline.map_or_else(
        || locator.resolve(cancellation),
        |deadline| locator.resolve_before(cancellation, deadline),
    ) {
        Ok(runtime) => runtime,
        Err(LocateError::Cancelled) => return Err(ProcessError::Cancelled),
        Err(LocateError::TimedOut) => {
            return Err(ProcessError::TimeoutBeforeSpawn {
                timeout_ms: request.timeout_ms(timeout_ceiling_ms),
            });
        }
        Err(LocateError::Unavailable(error)) => {
            return Err(ProcessError::Unavailable(error.to_string()));
        }
    };
    ensure_before_spawn(deadline, request.timeout_ms(timeout_ceiling_ms))?;
    tracing::info!(target: "agentshim", event = "process_resolve", phase = "execution");
    let cwd = spawn::resolve_cwd(root, request.cwd.as_deref()).map_err(invalid)?;
    ensure_before_spawn(deadline, request.timeout_ms(timeout_ceiling_ms))?;
    let resolved = ResolvedProgram {
        absolute: runtime.executable.clone(),
        executable: runtime.executable.clone(),
        launcher: launcher_for(&runtime.executable)?,
    };
    ensure_before_spawn(deadline, request.timeout_ms(timeout_ceiling_ms))?;
    let environment = bash_environment(&runtime, request.msys_argument_conversion);
    let args = bash_args(&request.command);
    if request.detach {
        let launch = DetachedLaunch {
            resolved: &resolved,
            cwd: &cwd,
            args: &args,
            environment: &environment,
        };
        let admission = detached_admission.ok_or_else(|| {
            ProcessError::ResourceBusy(
                "detached bash request reached execution without a reserved slot".to_owned(),
            )
        })?;
        return run_detached(
            root,
            admission,
            request,
            &launch,
            cancellation,
            output_budget,
            capture_sink.cloned(),
        );
    }
    let timeout = deadline
        .and_then(|deadline| deadline.checked_duration_since(std::time::Instant::now()))
        .filter(|remaining| !remaining.is_zero())
        .ok_or(ProcessError::TimeoutBeforeSpawn {
            timeout_ms: request.timeout_ms(timeout_ceiling_ms),
        })?;
    let prepared = PreparedBash {
        resolved,
        cwd,
        args,
        environment,
        timeout,
        request_timeout_ms: request.timeout_ms(timeout_ceiling_ms),
        msys_retry_available: msys_retry_available(request),
    };
    execute_prepared_bash(prepared, None, cancellation, output_budget, capture_sink)
}

/// One resolved foreground bash launch: the probed bash executable, its `-c`
/// argv, working directory, locale environment, and timeout.
#[derive(Clone, Debug)]
pub struct PreparedBash {
    pub resolved: ResolvedProgram,
    pub cwd: PathBuf,
    pub args: Vec<String>,
    pub environment: EnvironmentPlan,
    pub timeout: Duration,
    pub request_timeout_ms: u64,
    pub msys_retry_available: bool,
}

/// Resolve the probed bash and validate one foreground command launch without
/// spawning it, so a host can wrap the final argv in a sandbox first.
pub fn prepare_bash_foreground(
    root: &Arc<RepositoryRoot>,
    locator: &BashLocator,
    request: &BashRequest,
    timeout: Duration,
    timeout_ceiling_ms: u64,
    cancellation: &CancellationToken,
) -> Result<PreparedBash, ProcessError> {
    request.validate(timeout_ceiling_ms)?;
    let runtime = locator.resolve(cancellation).map_err(|error| match error {
        LocateError::Cancelled => ProcessError::Cancelled,
        LocateError::TimedOut => ProcessError::TimeoutBeforeSpawn {
            timeout_ms: request.timeout_ms(timeout_ceiling_ms),
        },
        LocateError::Unavailable(message) => ProcessError::Unavailable(message.to_string()),
    })?;
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
        timeout,
        request_timeout_ms: request.timeout_ms(timeout_ceiling_ms),
        msys_retry_available: msys_retry_available(request),
    })
}

/// Spawn one prepared foreground bash launch, merged streams, with the same
/// tree ownership, timeout, capture ceiling, and cancellation as other hosts.
pub fn execute_prepared_bash(
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
        timeout,
        request_timeout_ms,
        msys_retry_available,
    } = prepared;
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
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

fn ensure_before_spawn(
    deadline: Option<std::time::Instant>,
    timeout_ms: u64,
) -> Result<(), ProcessError> {
    if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
        Err(ProcessError::TimeoutBeforeSpawn { timeout_ms })
    } else {
        Ok(())
    }
}

struct DetachedLaunch<'a> {
    resolved: &'a ResolvedProgram,
    cwd: &'a std::path::Path,
    args: &'a [String],
    environment: &'a EnvironmentPlan,
}

struct ServerLogCleanup(Option<PathBuf>);

impl Drop for ServerLogCleanup {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A detached tree binds its lifetime to the server instance rather than to this call, so the
/// response reports only what the agent needs to find it again: the pid and the log file.
#[allow(
    clippy::too_many_lines,
    reason = "all detached capture variants share one committed launch transaction"
)]
fn run_detached(
    root: &Arc<RepositoryRoot>,
    mut admission: DetachedAdmission,
    request: &BashRequest,
    launch: &DetachedLaunch<'_>,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
    capture_sink: Option<Arc<dyn spawn::CaptureSink>>,
) -> Result<ToolOutput, ProcessError> {
    let DetachedLaunch {
        resolved,
        cwd,
        args,
        environment,
    } = *launch;
    if let Some(sink) = capture_sink {
        verify_detached_response(
            &detached_response(admission.job_id(), u32::MAX, Path::new("remote-capture")),
            cancellation,
            output_budget,
        )?;
        if cancellation.is_cancelled() {
            return Err(ProcessError::Cancelled);
        }
        let plan = ExecPlan {
            resolved,
            cwd,
            args,
            environment,
            stdin: None,
            streams: Streams::Merged,
            timeout: Duration::ZERO,
            capture_page_bytes: output_budget.page_bytes(),
        };
        let (tree, reader) = crate::platform::process::spawn_detached_capture(&plan, environment)?;
        let pid = tree.pid();
        let output = detached_response(admission.job_id(), pid, Path::new("remote-capture"));
        let rollback_deadline = admission.rollback_deadline();
        let drain = detached::start_remote_drain(reader, sink);
        let log_reader = detached::empty_log_reader()?;
        if let Err(tree) = admission.retain_remote(tree, log_reader, drain) {
            let mut tree = tree;
            tree.terminate_and_wait(rollback_deadline)?;
            return Err(ProcessError::Cancelled);
        }
        return Ok(output);
    }
    let (log_path, resolved_log, server_owned) = if request.server_capture {
        (detached::server_log_path(admission.job_id()), None, true)
    } else {
        let requested = request
            .log_path
            .as_deref()
            .ok_or_else(|| invalid("detach requires a log_path inside the repository"))?;
        let resolved_log = root
            .resolve(std::path::Path::new(requested))
            .map_err(|error| invalid(format!("invalid log_path: {error}")))?;
        let path = resolved_log.absolute().to_owned();
        (path, Some(resolved_log), false)
    };
    verify_detached_response(
        &detached_response(admission.job_id(), u32::MAX, &log_path),
        cancellation,
        output_budget,
    )?;
    admission.reserve_log_path(&log_path)?;
    #[cfg(test)]
    admission.before_open();
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    let log = if server_owned {
        detached::open_server_log(&log_path)?
    } else {
        detached::open_log(
            root,
            resolved_log
                .as_ref()
                .expect("repository log retains its resolved capability"),
        )?
    };
    let mut server_log_cleanup = ServerLogCleanup(server_owned.then(|| log_path.clone()));
    #[cfg(test)]
    admission.after_open();
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    let plan = ExecPlan {
        resolved,
        cwd,
        args,
        environment,
        stdin: None,
        streams: Streams::Merged,
        timeout: Duration::ZERO,
        capture_page_bytes: output_budget.page_bytes(),
    };
    tracing::info!(target: "agentshim", event = "process_spawn", phase = "execution", detached = true);
    #[cfg(test)]
    if let Some(error) = admission.injected_spawn_error() {
        return Err(error);
    }
    let tree = crate::platform::process::spawn_detached(&plan, environment, log.writer)?;
    let pid = tree.pid();
    let job_id = admission.job_id().to_owned();
    let output = detached_response(&job_id, pid, &log_path);
    // The spawn-to-commit window is the last place a cancellation or shutdown can race the
    // call. A tree that executed user code must never be adopted by a stopped roster, so a
    // rejection is rolled back here with a bounded, verified termination.
    let rollback_deadline = admission.rollback_deadline();
    let rejected = if let Err(error) =
        verify_detached_response(&output, cancellation, output_budget)
    {
        Some((tree, error))
    } else if cancellation.is_cancelled() {
        Some((tree, ProcessError::Cancelled))
    } else if let Err(tree) = admission.retain(tree, log_path.clone(), log.reader, server_owned) {
        Some((tree, ProcessError::Cancelled))
    } else {
        None
    };
    if let Some((mut tree, error)) = rejected {
        tree.terminate_and_wait(rollback_deadline)?;
        return Err(error);
    }
    server_log_cleanup.0 = None;
    Ok(output)
}

fn detached_response(job_id: &str, pid: u32, log_path: &Path) -> ToolOutput {
    let text = format!(
        "Detached: job_id={job_id} pid={pid} log=\"{}\" scope={}.",
        diagnostic_path(log_path),
        crate::tools::exec::containment_scope()
    );
    ToolOutput::new(text.clone()).with_structured(json!({
        "tool": "bash",
        "job": {
            "jobId": job_id,
            "pid": pid,
        }
    }))
}

fn verify_detached_response(
    output: &ToolOutput,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
) -> Result<(), ProcessError> {
    if output.fits_content_and_call(output_budget, cancellation) {
        return Ok(());
    }
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    if output.encoded_len()
        > crate::output::OutputLimits::for_content_within(&output.text, output_budget.page_bytes())
            .bytes
    {
        return Err(crate::output::OutputError::RequiredContentTooLarge.into());
    }
    Err(crate::output::OutputError::BurstLimit.into())
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
                        && matches!(
                            output_budget.project_result(
                                &render.text,
                                Some(&structured),
                                true,
                                cancellation
                            ),
                            crate::output::ProjectionDecision::Fits(_)
                        )
                })
        },
    )
    .map_err(|error| normalize_burst_render_error(error, output_budget))
}

fn normalize_burst_render_error(
    error: ProcessError,
    output_budget: &dyn crate::output::CallBudget,
) -> ProcessError {
    if output_budget.ceiling() < crate::output::CALL_OUTPUT_TOKEN_LIMIT
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
