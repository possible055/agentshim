//! The detached launch transaction: resolution, log reservation, spawn, and
//! the commit-or-rollback window that hands the tree to the roster.

use std::{path::Path, sync::Arc, time::Duration};

use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::DetachedAdmission;
use crate::{
    output::{OutputError, OutputLimits},
    path::RepositoryRoot,
    platform::process::spawn_detached,
    tools::{
        ToolOutput,
        bash::{
            BashRequest, bash_args, bash_environment, invalid,
            locate::{BashLocator, LocateError},
        },
        exec::{
            ProcessError,
            capture::diagnostic_path,
            resolve::{ResolvedProgram, launcher_for},
            spawn::{self, EnvironmentPlan, ExecPlan, Streams},
        },
    },
};

/// Resolve, reserve, spawn, and commit one detached command. Kept apart from
/// the foreground pipeline because the pre-spawn window runs against the
/// launch timeout rather than the background job timeout.
#[allow(
    clippy::too_many_lines,
    reason = "admission, reservation, and spawn share one transaction"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "the call carries the full launch transaction inputs"
)]
pub(in crate::tools::bash) fn execute_detached(
    root: &Arc<RepositoryRoot>,
    locator: &BashLocator,
    detached_admission: Option<DetachedAdmission>,
    request: &BashRequest,
    timeout: Duration,
    cancellation: &CancellationToken,
    timeout_ceiling_ms: u64,
    output_budget: &dyn crate::output::CallBudget,
) -> Result<ToolOutput, ProcessError> {
    let started = std::time::Instant::now();
    request.validate(timeout_ceiling_ms)?;
    let deadline = started + timeout;
    let pre_spawn_timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    let runtime = match locator.resolve_before(cancellation, deadline) {
        Ok(runtime) => runtime,
        Err(LocateError::Cancelled) => return Err(ProcessError::Cancelled),
        Err(LocateError::TimedOut) => {
            return Err(ProcessError::TimeoutBeforeSpawn {
                timeout_ms: pre_spawn_timeout_ms,
            });
        }
        Err(LocateError::Unavailable(error)) => {
            return Err(ProcessError::Unavailable(error.to_string()));
        }
    };
    ensure_before_spawn(Some(deadline), pre_spawn_timeout_ms)?;
    tracing::info!(target: "agentshim", event = "process_resolve", phase = "execution");
    let cwd = spawn::resolve_cwd(root, request.cwd.as_deref()).map_err(invalid)?;
    ensure_before_spawn(Some(deadline), pre_spawn_timeout_ms)?;
    let resolved = ResolvedProgram {
        absolute: runtime.executable.clone(),
        executable: runtime.executable.clone(),
        launcher: launcher_for(&runtime.executable)?,
    };
    ensure_before_spawn(Some(deadline), pre_spawn_timeout_ms)?;
    let environment = bash_environment(&runtime, request.msys_argument_conversion);
    let args = bash_args(&request.command);
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
    run_detached(
        root,
        admission,
        request,
        &launch,
        cancellation,
        output_budget,
        Duration::from_millis(request.background_timeout_ms(timeout_ceiling_ms)),
    )
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
    cwd: &'a Path,
    args: &'a [String],
    environment: &'a EnvironmentPlan,
}

/// A detached tree binds its lifetime to the server instance rather than to this call, so the
/// response reports only what the agent needs to find it again: the pid and the log file.
#[allow(
    clippy::too_many_lines,
    reason = "detached launch admission and commit share one transaction"
)]
fn run_detached(
    root: &Arc<RepositoryRoot>,
    mut admission: DetachedAdmission,
    request: &BashRequest,
    launch: &DetachedLaunch<'_>,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
    effective_timeout: Duration,
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
    verify_detached_response(
        &detached_response(admission.job_id(), u32::MAX, &log_path),
        cancellation,
        output_budget,
    )?;
    admission.reserve_log_path(&log_path)?;
    #[cfg(any(test, feature = "test-hooks"))]
    admission.before_open();
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    let log = super::open_log(root, &resolved_log)?;
    #[cfg(any(test, feature = "test-hooks"))]
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
    #[cfg(any(test, feature = "test-hooks"))]
    if let Some(error) = admission.injected_spawn_error() {
        return Err(error);
    }
    let tree = spawn_detached(&plan, environment, log.writer)?;
    let spawned_at = std::time::Instant::now();
    let pid = tree.pid();
    let job_id = admission.job_id().to_owned();
    let output = detached_response(&job_id, pid, &log_path);
    // The spawn-to-commit window is the last place a cancellation or shutdown can race the
    // call. A tree that executed user code must never be adopted by a stopped roster, so a
    // rejection is rolled back here with a bounded, verified termination.
    let rollback_deadline = admission.rollback_deadline();
    let rejected =
        if let Err(error) = verify_detached_response(&output, cancellation, output_budget) {
            Some((tree, error))
        } else if cancellation.is_cancelled() {
            Some((tree, ProcessError::Cancelled))
        } else if let Err(tree) = admission.retain(
            tree,
            log_path.clone(),
            log.reader,
            effective_timeout,
            spawned_at,
        ) {
            Some((tree, ProcessError::Cancelled))
        } else {
            None
        };
    if let Some((mut tree, error)) = rejected {
        tree.terminate_and_wait(rollback_deadline)?;
        return Err(error);
    }
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
        > OutputLimits::for_content_within(&output.text, output_budget.page_bytes()).bytes
    {
        return Err(OutputError::RequiredContentTooLarge.into());
    }
    Err(OutputError::BurstLimit.into())
}
