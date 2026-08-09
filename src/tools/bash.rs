use std::{path::PathBuf, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
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
            capture::{Capture, RenderedCapture, diagnostic_path, project_captures},
            platform,
            resolve::{ResolvedProgram, launcher_for},
            spawn::{
                self, DEFAULT_TIMEOUT_MS, EnvironmentPlan, ExecFailure, ExecPlan, MAX_TIMEOUT_MS,
                Streams,
            },
        },
    },
};

pub(crate) mod detached;
pub(crate) mod locate;
#[cfg(test)]
mod tests;

const BASH_MEMORY_BYTES: usize = 2 * 1024 * 1024;

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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BashRequest {
    pub command: String,
    pub cwd: Option<String>,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub detach: bool,
    pub log_path: Option<String>,
}

impl BashRequest {
    /// Validate scalar and combination constraints before process admission.
    ///
    /// # Errors
    ///
    /// Returns a validation error for an empty command, an out-of-range timeout, or a
    /// `detach`/`log_path`/`timeout_ms` combination the tool does not accept.
    pub fn validate(&self) -> Result<(), ProcessError> {
        if self.command.is_empty() {
            return Err(invalid("command must not be empty"));
        }
        if self.command.contains('\0') || self.cwd.as_deref().is_some_and(|cwd| cwd.contains('\0'))
        {
            return Err(invalid("command and cwd must not contain NUL"));
        }
        if self.detach {
            if self.log_path.as_deref().is_none_or(str::is_empty) {
                return Err(invalid("detach requires a log_path inside the repository"));
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
        if !(1..=MAX_TIMEOUT_MS).contains(&self.timeout_ms()) {
            return Err(invalid(format!(
                "timeout_ms must be from 1 to {MAX_TIMEOUT_MS}"
            )));
        }
        Ok(())
    }

    #[must_use]
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)
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

fn environment(runtime: &locate::BashRuntime) -> EnvironmentPlan {
    let mut plan = EnvironmentPlan::from_defaults(&BASH_ENVIRONMENT);
    plan.injected
        .push(("LANG".to_owned(), runtime.locale.clone()));
    plan.injected
        .push(("LC_ALL".to_owned(), runtime.locale.clone()));
    // An override rather than an injection: this must replace the inherited `PATH`, not sit
    // beside it under a differently cased key.
    if let Some(path) = &runtime.path {
        plan.overrides.push(("PATH".to_owned(), path.clone()));
    }
    plan
}

/// Resolve the probed bash and run one command line through it.
///
/// # Errors
///
/// Returns validation, bash-discovery, spawn, I/O, cancellation, timeout, cleanup, or output
/// errors.
pub(crate) fn execute_output(
    root: &Arc<RepositoryRoot>,
    locator: &BashLocator,
    detached_admission: Option<DetachedAdmission>,
    request: &BashRequest,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ProcessError> {
    let started = std::time::Instant::now();
    request.validate()?;
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
                timeout_ms: request.timeout_ms(),
            });
        }
        Err(LocateError::Unavailable(error)) => {
            return Err(ProcessError::Unavailable(error.to_string()));
        }
    };
    ensure_before_spawn(deadline, request.timeout_ms())?;
    tracing::info!(target: "codexshim", event = "process_resolve", phase = "execution");
    let cwd = spawn::resolve_cwd(root, request.cwd.as_deref()).map_err(invalid)?;
    ensure_before_spawn(deadline, request.timeout_ms())?;
    let resolved = ResolvedProgram {
        absolute: runtime.executable.clone(),
        executable: runtime.executable.clone(),
        launcher: launcher_for(&runtime.executable)?,
    };
    ensure_before_spawn(deadline, request.timeout_ms())?;
    let environment = environment(&runtime);
    let args = vec![
        "--noprofile".to_owned(),
        "--norc".to_owned(),
        "-c".to_owned(),
        request.command.clone(),
    ];
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
        return run_detached(root, admission, request, &launch, cancellation);
    }
    let timeout = deadline
        .and_then(|deadline| deadline.checked_duration_since(std::time::Instant::now()))
        .filter(|remaining| !remaining.is_zero())
        .ok_or(ProcessError::TimeoutBeforeSpawn {
            timeout_ms: request.timeout_ms(),
        })?;
    let plan = ExecPlan {
        resolved: &resolved,
        cwd: &cwd,
        args: &args,
        environment: &environment,
        stdin: None,
        streams: Streams::Merged,
        timeout,
    };
    match spawn::run(&plan, cancellation) {
        Ok(outcome) => render_completed(
            &CompletedBash {
                bash: resolved.absolute,
                cwd,
                exit: outcome.exit,
                duration: outcome.duration,
                output: expect_one(outcome.captures),
            },
            cancellation,
        ),
        Err(ExecFailure::TimedOut { duration, captures }) => {
            let timeout_ms = request.timeout_ms();
            let report = render_timeout(
                &TimedOutBash {
                    bash: resolved.absolute,
                    cwd,
                    duration,
                    output: expect_one(captures),
                },
                timeout_ms,
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

/// A detached tree binds its lifetime to the server instance rather than to this call, so the
/// response reports only what the agent needs to find it again: the pid and the log file.
fn run_detached(
    root: &Arc<RepositoryRoot>,
    admission: DetachedAdmission,
    request: &BashRequest,
    launch: &DetachedLaunch<'_>,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ProcessError> {
    let DetachedLaunch {
        resolved,
        cwd,
        args,
        environment,
    } = *launch;
    let requested = request
        .log_path
        .as_deref()
        .ok_or_else(|| invalid("detach requires a log_path inside the repository"))?;
    let resolved_log = root
        .resolve(std::path::Path::new(requested))
        .map_err(|error| invalid(format!("invalid log_path: {error}")))?;
    let log_path = resolved_log.absolute().to_owned();
    #[cfg(test)]
    admission.before_open();
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    let log = detached::open_log(root, &resolved_log)?;
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
    };
    tracing::info!(target: "codexshim", event = "process_spawn", phase = "execution", detached = true);
    #[cfg(test)]
    if let Some(error) = admission.injected_spawn_error() {
        return Err(error);
    }
    let tree = platform::spawn_detached(&plan, environment, log)?;
    let pid = tree.pid();
    admission.retain(tree, log_path.clone());
    let rendered = format!(
        "Bash: {}\nCwd: {}\nPid: {pid}\nLog path: {}\nDetached; lifecycle scope is {}.",
        diagnostic_path(&resolved.absolute),
        diagnostic_path(cwd),
        diagnostic_path(&log_path),
        crate::tools::exec::containment_scope()
    );
    Ok(ToolOutput::new(rendered))
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

fn render_completed(
    completed: &CompletedBash,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ProcessError> {
    project_captures(
        &[&completed.output],
        cancellation,
        |rendered| completed_output(completed, &rendered[0]),
        ToolOutput::fits_content_budget,
    )
}

fn completed_output(completed: &CompletedBash, output: &RenderedCapture) -> ToolOutput {
    let header = format!(
        "Bash: {}\nCwd: {}",
        diagnostic_path(&completed.bash),
        diagnostic_path(&completed.cwd)
    );
    let tail = [
        format!("Exit code: {}", completed.exit),
        format!("Duration ms: {}", completed.duration.as_millis()),
        format!(
            "Output bytes: total={}, shown={}, omitted={}, invalid={}, encoding={}",
            completed.output.bytes_read,
            output.shown_bytes,
            output.omitted_bytes,
            output.invalid_bytes,
            output.encoding
        ),
        "Complete.".to_owned(),
    ];
    let mut rendered = String::with_capacity(
        header
            .len()
            .saturating_add(output.text.len())
            .saturating_add(192),
    );
    rendered.push_str(&header);
    rendered.push_str("\n--- output ---\n");
    rendered.push_str(&output.text);
    for line in tail {
        rendered.push('\n');
        rendered.push_str(&line);
    }
    ToolOutput::with_child_nonzero(rendered, completed.exit != "0")
}

fn render_timeout(
    timed_out: &TimedOutBash,
    timeout_ms: u64,
) -> Result<TimeoutRender, ProcessError> {
    let cancellation = CancellationToken::new();
    project_captures(
        &[&timed_out.output],
        &cancellation,
        |rendered| timeout_output(timed_out, timeout_ms, &rendered[0]),
        |render: &TimeoutRender| {
            serde_json::to_value(&render.details)
                .ok()
                .is_some_and(|details| {
                    crate::output::tool_error_result_fits_content_budget(
                        "resource_timeout",
                        true,
                        &render.text,
                        Some(&details),
                    )
                })
        },
    )
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
