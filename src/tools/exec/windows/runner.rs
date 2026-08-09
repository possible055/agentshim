use std::{
    cmp::Ordering,
    ffi::{OsStr, OsString, c_void},
    fs::File,
    io,
    mem::{self, size_of},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, RawHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    thread,
    time::Instant,
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
    ProcessError,
    capture::{Capture, capture_bytes_per_stream, drain, write_stdin},
    resolve::{Launcher, ResolvedProgram},
    spawn::{
        CLEANUP_DEADLINE, DESCENDANT_EXIT_GRACE, EnvironmentPlan, ExecFailure, ExecOutcome,
        ExecPlan, Streams, ThreadCompletion, spawn_monitored,
    },
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering as AtomicOrdering},
};

const NATIVE_COMMAND_LINE_LIMIT: usize = 32_767;
const BATCH_COMMAND_LINE_LIMIT: usize = 8_191;
const TERMINATION_EXIT_CODE: u32 = 0xC0DE_CACE;

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
    plan: &ExecPlan<'_>,
    cancellation: &CancellationToken,
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
        return Err(ProcessError::from(io_context(
            "CreateProcessW",
            &io::Error::last_os_error(),
        ))
        .into());
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
    #[cfg(test)]
    inject_failure(FailurePoint::Running).map_err(ProcessError::Io)?;

    let stdin_file = stdin_pipe.parent.into_file();
    let input = plan.stdin.map(str::to_owned);
    let (io_failed, completion, stdin_thread, drains) =
        spawn_io_threads(stdin_file, output_files, input);

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

type IoThreads = (
    Arc<AtomicBool>,
    ThreadCompletion,
    thread::JoinHandle<io::Result<()>>,
    Vec<thread::JoinHandle<io::Result<Capture>>>,
);

type PendingIo = (
    ThreadCompletion,
    thread::JoinHandle<io::Result<()>>,
    Vec<thread::JoinHandle<io::Result<Capture>>>,
);

fn spawn_io_threads(stdin: File, outputs: Vec<File>, input: Option<String>) -> IoThreads {
    let failed = Arc::new(AtomicBool::new(false));
    let completion = ThreadCompletion::new();
    let capture_bytes = capture_bytes_per_stream(outputs.len());
    let stdin = spawn_monitored(Arc::clone(&failed), completion.clone(), move || {
        write_stdin(stdin, input.as_deref())
    });
    let drains = outputs
        .into_iter()
        .map(|output| {
            spawn_monitored(Arc::clone(&failed), completion.clone(), move || {
                drain(output, capture_bytes)
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
    stdin: thread::JoinHandle<io::Result<()>>,
    drains: Vec<thread::JoinHandle<io::Result<Capture>>>,
) -> Result<ExecOutcome, ExecFailure> {
    lifecycle.terminate_and_wait()?;
    let (stdin_result, _) = settle_threads(completion, stdin, drains)?;
    stdin_result.map_err(ProcessError::Io)?;
    lifecycle.finish();
    Err(ProcessError::Io(io::Error::other(
        "process I/O task failed without an error",
    ))
    .into())
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
