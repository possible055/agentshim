use std::{
    ffi::c_void,
    fs::File,
    io,
    ptr::null,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    thread,
    time::Instant,
};

use tokio_util::sync::CancellationToken;
use windows_sys::Win32::{
    Globalization::GetOEMCP,
    System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
    },
};

use super::platform::{
    LaunchEncoding, Lifecycle, PreparedStdio, create_process_cwd, environment_block, prepare_stdio,
    settle_threads,
};
use crate::tools::exec::{
    ProcessError,
    capture::{Capture, capture_bytes_per_stream, drain_with_capture, write_stdin},
    resolve::Launcher,
    spawn::{
        CaptureSink, DESCENDANT_EXIT_GRACE, ExecFailure, ExecOutcome, ExecPlan, ThreadCompletion,
        spawn_monitored,
    },
};

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailurePoint {
    SpawnedSuspended,
    JobReady,
    JobAssigned,
    Running,
}

#[cfg(test)]
thread_local! {
    pub static FAILURE_POINT: std::cell::Cell<Option<FailurePoint>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub fn inject_failure(point: FailurePoint) -> io::Result<()> {
    if FAILURE_POINT.with(std::cell::Cell::get) == Some(point) {
        Err(io::Error::other(format!(
            "injected lifecycle failure at {point:?}"
        )))
    } else {
        Ok(())
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the Windows launch path keeps setup, IO handoff, and teardown in one scope"
)]
pub fn run(
    plan: &ExecPlan<'_>,
    cancellation: &CancellationToken,
    capture_sink: Option<&Arc<dyn CaptureSink>>,
) -> Result<ExecOutcome, ExecFailure> {
    let started = Instant::now();
    let (launch, environment, cwd_wide) = prepare_launch_inputs(plan)?;

    let stdio = prepare_stdio(plan.streams).map_err(|error| io_context("prepare stdio", &error))?;

    let mut command_line = launch.command_line;
    let mut process_info = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            launch.application.as_ptr(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_UNICODE_ENVIRONMENT
                | CREATE_NO_WINDOW
                | CREATE_SUSPENDED
                | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast::<c_void>(),
            cwd_wide.as_ptr(),
            &raw const stdio.startup.StartupInfo,
            &raw mut process_info,
        )
    };
    if created == 0 {
        return Err(
            ProcessError::from(io_context("CreateProcessW", &io::Error::last_os_error())).into(),
        );
    }
    let mut lifecycle = Lifecycle::new(process_info)
        .map_err(|error| ProcessError::from(io_context("create lifecycle", &error)))?;
    #[cfg(test)]
    inject_failure(FailurePoint::SpawnedSuspended).map_err(ProcessError::Io)?;
    let PreparedStdio {
        stdin: stdin_pipe,
        outputs,
        attributes,
        startup: _startup,
    } = stdio;
    drop(attributes);
    drop(stdin_pipe.child);
    let mut output_files = Vec::with_capacity(outputs.len());
    for output in outputs {
        drop(output.child);
        output_files.push(output.parent.into_file());
    }

    lifecycle
        .install_job()
        .map_err(|error| ProcessError::from(io_context("install job", &error)))?;
    lifecycle
        .resume()
        .map_err(|error| ProcessError::from(io_context("resume primary process", &error)))?;
    trace_process_started(&lifecycle);
    #[cfg(test)]
    inject_failure(FailurePoint::Running).map_err(ProcessError::Io)?;

    let stdin_file = stdin_pipe.parent.into_file();
    let input = plan.stdin.map(str::to_owned);
    let (io_failed, completion, stdin_thread, drains) = spawn_io_threads(
        stdin_file,
        output_files,
        input,
        plan.resolved.launcher,
        plan.capture_page_bytes,
        capture_sink,
    );

    let mut primary_exit = None;
    let exit_code = loop {
        if io_failed.load(AtomicOrdering::Acquire) {
            return terminate_after_io_failure(&mut lifecycle, &completion, stdin_thread, drains);
        }
        lifecycle.poll_completion_hint(25);
        let primary = lifecycle.primary_exit_code().map_err(ProcessError::Io)?;
        let active = lifecycle.active_processes().map_err(ProcessError::Io)?;
        if let Some(code) = primary {
            let (code, detected_at) = primary_exit.get_or_insert((code, Instant::now()));
            if active == 0 {
                break *code;
            }
            if detected_at.elapsed() >= DESCENDANT_EXIT_GRACE {
                let code = *code;
                lifecycle.terminate_and_wait()?;
                break code;
            }
        }
        if cancellation.is_cancelled() {
            return terminate_after_cancellation(
                &mut lifecycle,
                (completion, stdin_thread, drains),
            );
        }
        if started.elapsed() >= plan.timeout {
            trace_timeout_state(&lifecycle, primary.is_none(), active);
            return terminate_after_timeout(
                &mut lifecycle,
                (completion, stdin_thread, drains),
                started,
            );
        }
    };

    finish_completed(
        &mut lifecycle,
        (completion, stdin_thread, drains),
        exit_code,
        started,
    )
}

fn trace_process_started(lifecycle: &Lifecycle) {
    tracing::info!(
        target: "agentshim",
        event = "process_started",
        phase = "execution",
        primary_pid = lifecycle.primary_pid(),
        containment = "job"
    );
}

fn trace_timeout_state(lifecycle: &Lifecycle, primary_running: bool, active_processes: u32) {
    tracing::error!(
        target: "agentshim",
        event = "process_timeout_state",
        phase = "cleanup",
        primary_pid = lifecycle.primary_pid(),
        primary_running,
        active_processes
    );
}

fn io_context(context: &str, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

fn prepare_launch_inputs(
    plan: &ExecPlan<'_>,
) -> Result<(LaunchEncoding, Vec<u16>, Vec<u16>), ProcessError> {
    Ok((
        LaunchEncoding::new(plan.resolved, plan.args)?,
        environment_block(plan.environment),
        create_process_cwd(plan.cwd)?,
    ))
}

pub type IoThreads = (
    Arc<AtomicBool>,
    ThreadCompletion,
    Option<thread::JoinHandle<io::Result<()>>>,
    Vec<thread::JoinHandle<io::Result<Capture>>>,
);

type PendingIo = (
    ThreadCompletion,
    Option<thread::JoinHandle<io::Result<()>>>,
    Vec<thread::JoinHandle<io::Result<Capture>>>,
);

pub fn spawn_io_threads(
    stdin: File,
    outputs: Vec<File>,
    input: Option<String>,
    launcher: Launcher,
    capture_page_bytes: usize,
    capture_sink: Option<&Arc<dyn CaptureSink>>,
) -> IoThreads {
    let failed = Arc::new(AtomicBool::new(false));
    let completion = ThreadCompletion::new();
    let capture_bytes = capture_bytes_per_stream(outputs.len(), capture_page_bytes);
    let oem_code_page = (launcher == Launcher::CmdCompat).then(|| unsafe { GetOEMCP() });
    let stdin = input.filter(|input| !input.is_empty()).map(|input| {
        spawn_monitored(Arc::clone(&failed), completion.clone(), move || {
            write_stdin(stdin, Some(&input))
        })
    });
    let drains = outputs
        .into_iter()
        .enumerate()
        .map(|(stream, output)| {
            let capture_sink = capture_sink.cloned();
            spawn_monitored(Arc::clone(&failed), completion.clone(), move || {
                drain_with_capture(
                    output,
                    capture_bytes,
                    oem_code_page,
                    capture_sink.as_deref(),
                    stream,
                )
            })
        })
        .collect();
    (failed, completion, stdin, drains)
}

fn finish_completed(
    lifecycle: &mut Lifecycle,
    (completion, stdin, drains): PendingIo,
    exit_code: u32,
    started: Instant,
) -> Result<ExecOutcome, ExecFailure> {
    let (stdin_result, captures) = settle_threads(&completion, stdin, drains)?;
    stdin_result.map_err(ProcessError::Io)?;
    lifecycle.finish();
    Ok(ExecOutcome {
        exit: exit_code.to_string(),
        duration: started.elapsed(),
        captures,
    })
}

fn terminate_after_timeout(
    lifecycle: &mut Lifecycle,
    (completion, stdin, drains): PendingIo,
    started: Instant,
) -> Result<ExecOutcome, ExecFailure> {
    lifecycle.terminate_and_wait()?;
    let (stdin_result, captures) = settle_threads(&completion, stdin, drains)?;
    let _ = stdin_result;
    lifecycle.finish();
    Err(ExecFailure::TimedOut {
        duration: started.elapsed(),
        captures,
    })
}

fn terminate_after_io_failure(
    lifecycle: &mut Lifecycle,
    completion: &ThreadCompletion,
    stdin: Option<thread::JoinHandle<io::Result<()>>>,
    drains: Vec<thread::JoinHandle<io::Result<Capture>>>,
) -> Result<ExecOutcome, ExecFailure> {
    lifecycle.terminate_and_wait()?;
    let (stdin_result, _) = settle_threads(completion, stdin, drains)?;
    stdin_result.map_err(ProcessError::Io)?;
    lifecycle.finish();
    Err(ProcessError::Io(io::Error::other("process I/O task failed without an error")).into())
}

fn terminate_after_cancellation(
    lifecycle: &mut Lifecycle,
    (completion, stdin, drains): PendingIo,
) -> Result<ExecOutcome, ExecFailure> {
    lifecycle.terminate_and_wait()?;
    let (stdin_result, _) = settle_threads(&completion, stdin, drains)?;
    let _ = stdin_result;
    lifecycle.finish();
    Err(ProcessError::Cancelled.into())
}
