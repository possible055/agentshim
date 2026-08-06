use std::{
    cmp::Ordering,
    ffi::{OsStr, OsString, c_void},
    fs::File,
    io,
    mem::{self, size_of},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{FromRawHandle, RawHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    thread,
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Globalization::{CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN, CompareStringOrdinal},
    Security::SECURITY_ATTRIBUTES,
    System::{
        IO::{CreateIoCompletionPort, GetQueuedCompletionStatus},
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectAssociateCompletionPortInformation,
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        },
        Pipes::CreatePipe,
        SystemInformation::GetSystemDirectoryW,
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
            InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
            TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
        },
    },
};

use super::{
    CLEANUP_DEADLINE, Capture, CompletedProcess, ENVIRONMENT_DEFAULTS, Launcher, ProcessError,
    ProcessRequest, ResolvedProgram, ThreadCompletion, TimedOutProcess, drain, render_completed,
    render_timeout, spawn_monitored, write_stdin, ToolOutput,
};
use std::sync::{Arc, atomic::Ordering as AtomicOrdering};

const NATIVE_COMMAND_LINE_LIMIT: usize = 32_767;
const BATCH_COMMAND_LINE_LIMIT: usize = 8_191;
const TERMINATION_EXIT_CODE: u32 = 0xC0DE_CACE;
const DESCENDANT_EXIT_GRACE: Duration = Duration::from_millis(250);

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailurePoint {
    SpawnedSuspended,
    JobReady,
    JobAssigned,
    Running,
}

#[cfg(test)]
thread_local! {
    static FAILURE_POINT: std::cell::Cell<Option<FailurePoint>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn inject_failure(point: FailurePoint) -> io::Result<()> {
    if FAILURE_POINT.with(std::cell::Cell::get) == Some(point) {
        Err(io::Error::other(format!(
            "injected lifecycle failure at {point:?}"
        )))
    } else {
        Ok(())
    }
}

pub(super) fn run(
    resolved: &ResolvedProgram,
    cwd: &Path,
    request: &ProcessRequest,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ProcessError> {
    let started = Instant::now();
    let (launch, environment, cwd_wide) = prepare_launch_inputs(resolved, cwd, request)?;

    let (stdin_pipe, stdout_pipe, stderr_pipe, attributes, startup) =
        prepare_stdio().map_err(|error| io_context("prepare stdio", &error))?;

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
            &raw const startup.StartupInfo,
            &raw mut process_info,
        )
    };
    if created == 0 {
        return Err(io_context("CreateProcessW", &io::Error::last_os_error()).into());
    }
    let mut lifecycle =
        Lifecycle::new(process_info).map_err(|error| io_context("create lifecycle", &error))?;
    #[cfg(test)]
    inject_failure(FailurePoint::SpawnedSuspended)?;
    drop(attributes);
    drop(stdin_pipe.child);
    drop(stdout_pipe.child);
    drop(stderr_pipe.child);

    lifecycle
        .install_job()
        .map_err(|error| io_context("install job", &error))?;
    lifecycle
        .resume()
        .map_err(|error| io_context("resume primary process", &error))?;
    #[cfg(test)]
    inject_failure(FailurePoint::Running)?;

    let stdin_file = stdin_pipe.parent.into_file();
    let stdout_file = stdout_pipe.parent.into_file();
    let stderr_file = stderr_pipe.parent.into_file();
    let input = request.stdin.clone();
    let (io_failed, completion, stdin_thread, stdout_thread, stderr_thread) =
        spawn_io_threads(stdin_file, stdout_file, stderr_file, input);

    let mut primary_exit = None;
    let exit_code = loop {
        if io_failed.load(AtomicOrdering::Acquire) {
            return terminate_after_io_failure(
                &mut lifecycle,
                &completion,
                stdin_thread,
                stdout_thread,
                stderr_thread,
            );
        }
        lifecycle.poll_completion_hint(25);
        let primary = lifecycle.primary_exit_code()?;
        let active = lifecycle.active_processes()?;
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
                (completion, stdin_thread, stdout_thread, stderr_thread),
            );
        }
        if started.elapsed() >= timeout {
            return terminate_after_timeout(
                &mut lifecycle,
                (completion, stdin_thread, stdout_thread, stderr_thread),
                resolved,
                cwd,
                request,
                started,
            );
        }
    };

    finish_completed(
        &mut lifecycle,
        (completion, stdin_thread, stdout_thread, stderr_thread),
        resolved,
        cwd,
        exit_code,
        started,
        cancellation,
    )
}

fn io_context(context: &str, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

fn prepare_launch_inputs(
    resolved: &ResolvedProgram,
    cwd: &Path,
    request: &ProcessRequest,
) -> Result<(LaunchEncoding, Vec<u16>, Vec<u16>), ProcessError> {
    Ok((
        LaunchEncoding::new(resolved, request)?,
        environment_block(request),
        create_process_cwd(cwd)?,
    ))
}

type IoThreads = (
    Arc<super::AtomicBool>,
    ThreadCompletion,
    thread::JoinHandle<io::Result<()>>,
    thread::JoinHandle<io::Result<Capture>>,
    thread::JoinHandle<io::Result<Capture>>,
);

type PendingIo = (
    ThreadCompletion,
    thread::JoinHandle<io::Result<()>>,
    thread::JoinHandle<io::Result<Capture>>,
    thread::JoinHandle<io::Result<Capture>>,
);

fn spawn_io_threads(stdin: File, stdout: File, stderr: File, input: Option<String>) -> IoThreads {
    let failed = Arc::new(super::AtomicBool::new(false));
    let completion = ThreadCompletion::new();
    let stdin = spawn_monitored(Arc::clone(&failed), completion.clone(), move || {
        write_stdin(stdin, input.as_deref())
    });
    let stdout = spawn_monitored(Arc::clone(&failed), completion.clone(), move || {
        drain(stdout)
    });
    let stderr = spawn_monitored(Arc::clone(&failed), completion.clone(), move || {
        drain(stderr)
    });
    (failed, completion, stdin, stdout, stderr)
}

fn finish_completed(
    lifecycle: &mut Lifecycle,
    (completion, stdin, stdout, stderr): PendingIo,
    resolved: &ResolvedProgram,
    cwd: &Path,
    exit_code: u32,
    started: Instant,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ProcessError> {
    let (stdin_result, stdout, stderr) = settle_threads(&completion, stdin, stdout, stderr)?;
    stdin_result?;
    lifecycle.finish();
    render_completed(
        &CompletedProcess {
            resolved: resolved.clone(),
            cwd: cwd.to_owned(),
            exit: exit_code.to_string(),
            duration: started.elapsed(),
            stdout,
            stderr,
        },
        cancellation,
    )
}

fn terminate_after_timeout(
    lifecycle: &mut Lifecycle,
    (completion, stdin, stdout, stderr): PendingIo,
    resolved: &ResolvedProgram,
    cwd: &Path,
    request: &ProcessRequest,
    started: Instant,
) -> Result<ToolOutput, ProcessError> {
    lifecycle.terminate_and_wait()?;
    let (stdin_result, stdout, stderr) = settle_threads(&completion, stdin, stdout, stderr)?;
    let _ = stdin_result;
    lifecycle.finish();
    let timeout_ms = request.timeout_ms();
    let timeout = render_timeout(
        &TimedOutProcess {
            resolved: resolved.clone(),
            cwd: cwd.to_owned(),
            duration: started.elapsed(),
            stdout,
            stderr,
        },
        timeout_ms,
    )?;
    Err(ProcessError::Timeout {
        timeout_ms,
        report: timeout.text,
        details: Box::new(timeout.details),
    })
}

fn terminate_after_io_failure(
    lifecycle: &mut Lifecycle,
    completion: &ThreadCompletion,
    stdin: thread::JoinHandle<io::Result<()>>,
    stdout: thread::JoinHandle<io::Result<Capture>>,
    stderr: thread::JoinHandle<io::Result<Capture>>,
) -> Result<ToolOutput, ProcessError> {
    lifecycle.terminate_and_wait()?;
    let (stdin_result, _, _) = settle_threads(completion, stdin, stdout, stderr)?;
    stdin_result?;
    lifecycle.finish();
    Err(ProcessError::Io(io::Error::other(
        "process I/O task failed without an error",
    )))
}

fn terminate_after_cancellation(
    lifecycle: &mut Lifecycle,
    (completion, stdin, stdout, stderr): PendingIo,
) -> Result<ToolOutput, ProcessError> {
    lifecycle.terminate_and_wait()?;
    let (stdin_result, _, _) = settle_threads(&completion, stdin, stdout, stderr)?;
    let _ = stdin_result;
    lifecycle.finish();
    Err(ProcessError::Cancelled)
}
