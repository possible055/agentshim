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
    ProcessRequest, ResolvedProgram, drain, render_completed, spawn_monitored, write_stdin,
};
use std::sync::{Arc, atomic::Ordering as AtomicOrdering};

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
    resolved: &ResolvedProgram,
    cwd: &Path,
    request: &ProcessRequest,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<String, ProcessError> {
    let started = Instant::now();
    let launch = LaunchEncoding::new(resolved, request)?;
    let environment = environment_block(request);
    let cwd_wide = create_process_cwd(cwd)?;

    let (stdin_pipe, stdout_pipe, stderr_pipe, attributes, startup) = prepare_stdio()?;

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
        return Err(io::Error::last_os_error().into());
    }
    let mut lifecycle = Lifecycle::new(process_info)?;
    #[cfg(test)]
    inject_failure(FailurePoint::SpawnedSuspended)?;
    drop(attributes);
    drop(stdin_pipe.child);
    drop(stdout_pipe.child);
    drop(stderr_pipe.child);

    lifecycle.install_job()?;
    lifecycle.resume()?;
    #[cfg(test)]
    inject_failure(FailurePoint::Running)?;

    let stdin_file = stdin_pipe.parent.into_file();
    let stdout_file = stdout_pipe.parent.into_file();
    let stderr_file = stderr_pipe.parent.into_file();
    let input = request.stdin.clone();
    let (io_failed, stdin_thread, stdout_thread, stderr_thread) =
        spawn_io_threads(stdin_file, stdout_file, stderr_file, input);

    let exit_code = loop {
        if io_failed.load(AtomicOrdering::Acquire) {
            return terminate_after_io_failure(
                &mut lifecycle,
                stdin_thread,
                stdout_thread,
                stderr_thread,
            );
        }
        lifecycle.poll_completion_hint();
        let primary = lifecycle.primary_exit_code()?;
        let active = lifecycle.active_processes()?;
        if let Some(code) = primary {
            if active == 0 {
                break code;
            }
        }
        if cancellation.is_cancelled() {
            lifecycle.terminate_and_wait()?;
            let (stdin_result, _, _) = settle_threads(stdin_thread, stdout_thread, stderr_thread)?;
            let _ = stdin_result;
            lifecycle.finish();
            return Err(ProcessError::Cancelled);
        }
        if started.elapsed() >= timeout {
            lifecycle.terminate_and_wait()?;
            let (stdin_result, _, _) = settle_threads(stdin_thread, stdout_thread, stderr_thread)?;
            let _ = stdin_result;
            lifecycle.finish();
            return Err(ProcessError::Timeout {
                timeout_ms: request.timeout_ms(),
            });
        }
        thread::sleep(Duration::from_millis(10));
    };

    let (stdin_result, stdout, stderr) =
        settle_threads(stdin_thread, stdout_thread, stderr_thread)?;
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

type IoThreads = (
    Arc<super::AtomicBool>,
    thread::JoinHandle<io::Result<()>>,
    thread::JoinHandle<io::Result<Capture>>,
    thread::JoinHandle<io::Result<Capture>>,
);

fn spawn_io_threads(stdin: File, stdout: File, stderr: File, input: Option<String>) -> IoThreads {
    let failed = Arc::new(super::AtomicBool::new(false));
    let stdin = spawn_monitored(Arc::clone(&failed), move || {
        write_stdin(stdin, input.as_deref())
    });
    let stdout = spawn_monitored(Arc::clone(&failed), move || drain(stdout));
    let stderr = spawn_monitored(Arc::clone(&failed), move || drain(stderr));
    (failed, stdin, stdout, stderr)
}

fn terminate_after_io_failure(
    lifecycle: &mut Lifecycle,
    stdin: thread::JoinHandle<io::Result<()>>,
    stdout: thread::JoinHandle<io::Result<Capture>>,
    stderr: thread::JoinHandle<io::Result<Capture>>,
) -> Result<String, ProcessError> {
    lifecycle.terminate_and_wait()?;
    let (stdin_result, _, _) = settle_threads(stdin, stdout, stderr)?;
    stdin_result?;
    lifecycle.finish();
    Err(ProcessError::Io(io::Error::other(
        "process I/O task failed without an error",
    )))
}

type PreparedStdio = (Pipe, Pipe, Pipe, AttributeList, STARTUPINFOEXW);

fn prepare_stdio() -> io::Result<PreparedStdio> {
    let stdin = Pipe::stdin()?;
    let stdout = Pipe::stdout()?;
    let stderr = Pipe::stdout()?;
    let inherited = [stdin.child.raw(), stdout.child.raw(), stderr.child.raw()];
    let attributes = AttributeList::new(&inherited)?;
    let startup = startup_info(inherited, &attributes)?;
    Ok((stdin, stdout, stderr, attributes, startup))
}

fn startup_info(inherited: [HANDLE; 3], attributes: &AttributeList) -> io::Result<STARTUPINFOEXW> {
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>())
        .map_err(|_| io::Error::other("STARTUPINFOEXW size overflow"))?;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = inherited[0];
    startup.StartupInfo.hStdOutput = inherited[1];
    startup.StartupInfo.hStdError = inherited[2];
    startup.lpAttributeList = attributes.as_ptr();
    Ok(startup)
}

type ThreadResults = (io::Result<()>, Capture, Capture);

fn settle_threads(
    stdin: thread::JoinHandle<io::Result<()>>,
    stdout: thread::JoinHandle<io::Result<Capture>>,
    stderr: thread::JoinHandle<io::Result<Capture>>,
) -> Result<ThreadResults, ProcessError> {
    let started = Instant::now();
    while !(stdin.is_finished() && stdout.is_finished() && stderr.is_finished()) {
        if started.elapsed() >= CLEANUP_DEADLINE {
            return Err(ProcessError::OutcomeUncertain);
        }
        thread::sleep(Duration::from_millis(10));
    }
    let stdin = stdin
        .join()
        .map_err(|_| io::Error::other("stdin writer panicked"))?;
    let stdout = stdout
        .join()
        .map_err(|_| io::Error::other("stdout drainer panicked"))??;
    let stderr = stderr
        .join()
        .map_err(|_| io::Error::other("stderr drainer panicked"))??;
    Ok((stdin, stdout, stderr))
}

struct LaunchEncoding {
    application: Vec<u16>,
    command_line: Vec<u16>,
}

impl LaunchEncoding {
    fn new(resolved: &ResolvedProgram, request: &ProcessRequest) -> Result<Self, ProcessError> {
        match resolved.launcher {
            Launcher::Native => {
                let application = nul_terminated(resolved.executable.as_os_str());
                let mut command_line = Vec::new();
                append_native_argv0(&mut command_line, resolved.absolute.as_os_str());
                for argument in &request.args {
                    command_line.push(u16::from(b' '));
                    append_native_argument(&mut command_line, OsStr::new(argument));
                }
                finish_native_command_line(command_line).map(|command_line| Self {
                    application,
                    command_line,
                })
            }
            Launcher::CmdCompat => {
                let command = system_cmd()?;
                let application = nul_terminated(command.as_os_str());
                let script = batch_user_path(&resolved.executable)?;
                let mut command_line = "cmd.exe /e:ON /v:OFF /d /c \"\""
                    .encode_utf16()
                    .collect::<Vec<_>>();
                command_line.extend(script);
                command_line.push(u16::from(b'"'));
                for argument in &request.args {
                    command_line.push(u16::from(b' '));
                    append_batch_argument(&mut command_line, argument)?;
                }
                command_line.push(u16::from(b'"'));
                finish_batch_command_line(command_line).map(|command_line| Self {
                    application,
                    command_line,
                })
            }
        }
    }
}

fn finish_native_command_line(mut encoded: Vec<u16>) -> Result<Vec<u16>, ProcessError> {
    if encoded.len().saturating_add(1) > NATIVE_COMMAND_LINE_LIMIT {
        return Err(ProcessError::Validation(format!(
            "encoded command line exceeds the {NATIVE_COMMAND_LINE_LIMIT} UTF-16 code-unit limit"
        )));
    }
    encoded.push(0);
    Ok(encoded)
}

fn finish_batch_command_line(mut encoded: Vec<u16>) -> Result<Vec<u16>, ProcessError> {
    if encoded.len() > BATCH_COMMAND_LINE_LIMIT {
        return Err(ProcessError::Validation(format!(
            "encoded batch command line exceeds the {BATCH_COMMAND_LINE_LIMIT} UTF-16 code-unit limit"
        )));
    }
    encoded.push(0);
    Ok(encoded)
}

fn append_native_argument(output: &mut Vec<u16>, argument: &OsStr) {
    let encoded = argument.encode_wide().collect::<Vec<_>>();
    let quote = encoded.is_empty() || encoded.iter().any(|unit| matches!(*unit, 0x09 | 0x20));
    if quote {
        output.push(u16::from(b'"'));
    }
    let mut backslashes = 0_usize;
    for unit in encoded {
        if unit == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }
        if unit == u16::from(b'"') {
            output.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
            output.push(unit);
        } else {
            output.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
            output.push(unit);
        }
        backslashes = 0;
    }
    let trailing_backslashes = if quote { backslashes * 2 } else { backslashes };
    output.extend(std::iter::repeat_n(u16::from(b'\\'), trailing_backslashes));
    if quote {
        output.push(u16::from(b'"'));
    }
}

fn append_native_argv0(output: &mut Vec<u16>, argument: &OsStr) {
    output.push(u16::from(b'"'));
    output.extend(argument.encode_wide());
    output.push(u16::from(b'"'));
}

fn append_batch_argument(output: &mut Vec<u16>, argument: &str) -> Result<(), ProcessError> {
    const UNQUOTED: &str = r"#$*+-./:?@\_";

    if argument.contains(['\0', '\r', '\n']) {
        return Err(ProcessError::Validation(
            "cmd-compat arguments must not contain NUL, CR, or LF".to_owned(),
        ));
    }
    let quote = argument.is_empty()
        || argument.ends_with('\\')
        || argument.chars().any(|character| {
            (character.is_ascii()
                && !(character.is_ascii_alphanumeric() || UNQUOTED.contains(character)))
                || character.is_control()
        });
    if quote {
        output.push(u16::from(b'"'));
    }
    let mut backslashes = 0_usize;
    for unit in argument.encode_utf16() {
        if unit == u16::from(b'\\') {
            backslashes += 1;
        } else {
            if unit == u16::from(b'"') {
                output.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
                output.push(u16::from(b'"'));
            } else if unit == u16::from(b'%') {
                append_ascii(output, "%%cd:~,");
            }
            backslashes = 0;
        }
        output.push(unit);
    }
    if quote {
        output.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
        output.push(u16::from(b'"'));
    }
    Ok(())
}

fn append_ascii(output: &mut Vec<u16>, value: &str) {
    output.extend(value.bytes().map(u16::from));
}

fn system_cmd() -> Result<PathBuf, ProcessError> {
    let mut buffer = vec![0_u16; 32_768];
    let length = unsafe {
        GetSystemDirectoryW(
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len()).expect("system directory buffer fits u32"),
        )
    };
    if length == 0 || usize::try_from(length).unwrap_or(usize::MAX) >= buffer.len() {
        return Err(io::Error::last_os_error().into());
    }
    buffer.truncate(length as usize);
    let mut directory = PathBuf::from(OsString::from_wide(&buffer));
    directory.push("cmd.exe");
    Ok(directory)
}

fn nul_terminated(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn create_process_cwd(path: &Path) -> Result<Vec<u16>, ProcessError> {
    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let user = strip_verbatim_prefix(&encoded).unwrap_or(encoded);
    if user.len().saturating_add(1) > NATIVE_COMMAND_LINE_LIMIT {
        return Err(ProcessError::Validation(
            "cwd exceeds the CreateProcessW UTF-16 limit".to_owned(),
        ));
    }
    Ok(user.into_iter().chain(Some(0)).collect())
}

fn batch_user_path(path: &Path) -> Result<Vec<u16>, ProcessError> {
    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let user = strip_verbatim_prefix(&encoded).unwrap_or(encoded);
    if user.len() >= 260 || user.last() == Some(&u16::from(b'\\')) {
        return Err(ProcessError::Validation(
            "cmd-compat does not accept verbatim or long batch paths".to_owned(),
        ));
    }
    Ok(user)
}

fn strip_verbatim_prefix(path: &[u16]) -> Option<Vec<u16>> {
    let unc_prefix = r"\\?\UNC\".encode_utf16().collect::<Vec<_>>();
    if path.starts_with(&unc_prefix) {
        let mut user = r"\\".encode_utf16().collect::<Vec<_>>();
        user.extend_from_slice(&path[unc_prefix.len()..]);
        return Some(user);
    }
    let drive_prefix = r"\\?\".encode_utf16().collect::<Vec<_>>();
    if path.starts_with(&drive_prefix) {
        return Some(path[drive_prefix.len()..].to_vec());
    }
    None
}

fn environment_block(request: &ProcessRequest) -> Vec<u16> {
    let mut variables = std::env::vars_os().collect::<Vec<_>>();
    for (key, value) in ENVIRONMENT_DEFAULTS {
        set_environment(&mut variables, OsStr::new(key), OsStr::new(value));
    }
    for key in &request.unset_env {
        variables.retain(|(existing, _)| !environment_key_equal(existing, OsStr::new(key)));
    }
    for (key, value) in &request.env {
        set_environment(&mut variables, OsStr::new(key), OsStr::new(value));
    }
    variables.sort_by(|left, right| environment_key_order(&left.0, &right.0));
    let mut block = Vec::new();
    for (key, value) in variables {
        block.extend(key.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

fn set_environment(variables: &mut Vec<(OsString, OsString)>, key: &OsStr, value: &OsStr) {
    variables.retain(|(existing, _)| !environment_key_equal(existing, key));
    variables.push((key.to_owned(), value.to_owned()));
}

fn environment_key_equal(left: &OsStr, right: &OsStr) -> bool {
    environment_key_order(left, right) == Ordering::Equal
}

fn environment_key_order(left: &OsStr, right: &OsStr) -> Ordering {
    let left = left.encode_wide().collect::<Vec<_>>();
    let right = right.encode_wide().collect::<Vec<_>>();
    let left_length = i32::try_from(left.len()).unwrap_or(i32::MAX);
    let right_length = i32::try_from(right.len()).unwrap_or(i32::MAX);
    match unsafe {
        CompareStringOrdinal(left.as_ptr(), left_length, right.as_ptr(), right_length, 1)
    } {
        CSTR_LESS_THAN => Ordering::Less,
        CSTR_EQUAL => Ordering::Equal,
        CSTR_GREATER_THAN => Ordering::Greater,
        _ => left.cmp(&right),
    }
}

struct Pipe {
    parent: OwnedHandle,
    child: OwnedHandle,
}

impl Pipe {
    fn stdin() -> io::Result<Self> {
        Self::create(true)
    }

    fn stdout() -> io::Result<Self> {
        Self::create(false)
    }

    fn create(parent_writes: bool) -> io::Result<Self> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
                .map_err(|_| io::Error::other("SECURITY_ATTRIBUTES size overflow"))?,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let mut read = null_mut();
        let mut write = null_mut();
        if unsafe { CreatePipe(&raw mut read, &raw mut write, &raw const attributes, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let read = OwnedHandle::new(read)?;
        let write = OwnedHandle::new(write)?;
        let (parent, child) = if parent_writes {
            (write, read)
        } else {
            (read, write)
        };
        if unsafe { SetHandleInformation(parent.raw(), HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { parent, child })
    }
}

struct AttributeList {
    storage: Vec<usize>,
}

impl AttributeList {
    fn new(handles: &[HANDLE]) -> io::Result<Self> {
        let mut bytes = 0_usize;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &raw mut bytes);
        }
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let words = bytes.div_ceil(size_of::<usize>());
        let storage = vec![0_usize; words];
        let pointer = storage.as_ptr().cast_mut().cast::<c_void>();
        if unsafe { InitializeProcThreadAttributeList(pointer, 1, 0, &raw mut bytes) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let list = Self { storage };
        if unsafe {
            UpdateProcThreadAttribute(
                list.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast::<c_void>(),
                mem::size_of_val(handles),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(list)
    }

    fn as_ptr(&self) -> *mut c_void {
        self.storage.as_ptr().cast_mut().cast::<c_void>()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.as_ptr());
        }
    }
}

struct Lifecycle {
    process: OwnedHandle,
    thread: Option<OwnedHandle>,
    job: Option<OwnedHandle>,
    completion: Option<OwnedHandle>,
    state: LifecycleState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifecycleState {
    SpawnedSuspended,
    JobAssigned,
    Running,
    Complete,
}

impl Lifecycle {
    fn new(info: PROCESS_INFORMATION) -> io::Result<Self> {
        let process = OwnedHandle::new(info.hProcess)?;
        let thread = match OwnedHandle::new(info.hThread) {
            Ok(thread) => thread,
            Err(error) => {
                unsafe {
                    TerminateProcess(process.raw(), TERMINATION_EXIT_CODE);
                }
                return Err(error);
            }
        };
        Ok(Self {
            process,
            thread: Some(thread),
            job: None,
            completion: None,
            state: LifecycleState::SpawnedSuspended,
        })
    }

    fn install_job(&mut self) -> io::Result<()> {
        if self.state != LifecycleState::SpawnedSuspended {
            return Err(io::Error::other(
                "invalid lifecycle transition to JobAssigned",
            ));
        }
        let job = OwnedHandle::new(unsafe { CreateJobObjectW(null(), null()) })?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        set_job_information(job.raw(), JobObjectExtendedLimitInformation, &limits)?;
        let completion = OwnedHandle::new(unsafe {
            CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1)
        })?;
        let association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
            CompletionKey: null_mut(),
            CompletionPort: completion.raw(),
        };
        set_job_information(
            job.raw(),
            JobObjectAssociateCompletionPortInformation,
            &association,
        )?;
        self.job = Some(job);
        self.completion = Some(completion);
        #[cfg(test)]
        inject_failure(FailurePoint::JobReady)?;
        if unsafe {
            AssignProcessToJobObject(
                self.job.as_ref().expect("job installed").raw(),
                self.process.raw(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        self.state = LifecycleState::JobAssigned;
        #[cfg(test)]
        inject_failure(FailurePoint::JobAssigned)?;
        Ok(())
    }

    fn resume(&mut self) -> io::Result<()> {
        if self.state != LifecycleState::JobAssigned {
            return Err(io::Error::other("invalid lifecycle transition to Running"));
        }
        let thread = self
            .thread
            .as_ref()
            .ok_or_else(|| io::Error::other("primary thread handle is unavailable"))?;
        if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        self.thread = None;
        self.state = LifecycleState::Running;
        Ok(())
    }

    fn primary_exit_code(&self) -> io::Result<Option<u32>> {
        let wait = unsafe { WaitForSingleObject(self.process.raw(), 0) };
        match wait {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => return Ok(None),
            _ => return Err(io::Error::last_os_error()),
        }
        let mut code = 0_u32;
        if unsafe { GetExitCodeProcess(self.process.raw(), &raw mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(code))
    }

    fn active_processes(&self) -> io::Result<u32> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let job = self
            .job
            .as_ref()
            .ok_or_else(|| io::Error::other("job handle is unavailable"))?;
        if unsafe {
            QueryInformationJobObject(
                job.raw(),
                JobObjectBasicAccountingInformation,
                (&raw mut accounting).cast::<c_void>(),
                u32::try_from(size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>())
                    .map_err(|_| io::Error::other("job accounting size overflow"))?,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(accounting.ActiveProcesses)
    }

    fn poll_completion_hint(&self) {
        let Some(completion) = &self.completion else {
            return;
        };
        let mut transferred = 0_u32;
        let mut key = 0_usize;
        let mut overlapped = null_mut();
        unsafe {
            GetQueuedCompletionStatus(
                completion.raw(),
                &raw mut transferred,
                &raw mut key,
                &raw mut overlapped,
                0,
            );
        }
    }

    fn terminate_and_wait(&mut self) -> Result<(), ProcessError> {
        let job = self
            .job
            .as_ref()
            .ok_or_else(|| io::Error::other("job handle is unavailable"))?;
        if unsafe { TerminateJobObject(job.raw(), TERMINATION_EXIT_CODE) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let started = Instant::now();
        while started.elapsed() < CLEANUP_DEADLINE {
            self.poll_completion_hint();
            if self.primary_exit_code()?.is_some() && self.active_processes()? == 0 {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(ProcessError::OutcomeUncertain)
    }

    fn finish(&mut self) {
        debug_assert_eq!(self.state, LifecycleState::Running);
        self.state = LifecycleState::Complete;
    }
}

impl Drop for Lifecycle {
    fn drop(&mut self) {
        if self.state == LifecycleState::Complete {
            return;
        }
        unsafe {
            if matches!(
                self.state,
                LifecycleState::JobAssigned | LifecycleState::Running
            ) {
                if let Some(job) = &self.job {
                    TerminateJobObject(job.raw(), TERMINATION_EXIT_CODE);
                }
            } else {
                TerminateProcess(self.process.raw(), TERMINATION_EXIT_CODE);
            }
        }
    }
}

fn set_job_information<T>(handle: HANDLE, class: i32, value: &T) -> io::Result<()> {
    if unsafe {
        SetInformationJobObject(
            handle,
            class,
            std::ptr::from_ref(value).cast::<c_void>(),
            u32::try_from(size_of::<T>())
                .map_err(|_| io::Error::other("job information size overflow"))?,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct OwnedHandle(HANDLE);

// The wrapper owns exactly one HANDLE and transfers that ownership when moved to a drain thread.
unsafe impl Send for OwnedHandle {}

impl OwnedHandle {
    fn new(handle: HANDLE) -> io::Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }

    fn raw(&self) -> HANDLE {
        self.0
    }

    fn into_file(self) -> File {
        let raw = self.0;
        mem::forget(self);
        unsafe { File::from_raw_handle(raw as RawHandle) }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsStr, path::PathBuf};

    use super::super::{launcher_for, validate_launcher_request};
    use super::{
        BATCH_COMMAND_LINE_LIMIT, FAILURE_POINT, FailurePoint, LaunchEncoding, Launcher,
        ProcessRequest, ResolvedProgram, append_native_argument, append_native_argv0,
        finish_batch_command_line, finish_native_command_line, run,
    };

    fn fixture_request(args: Vec<&str>) -> ProcessRequest {
        ProcessRequest {
            program: "fixture".to_owned(),
            args: args.into_iter().map(str::to_owned).collect(),
            cwd: None,
            env: BTreeMap::new(),
            unset_env: Vec::new(),
            stdin: None,
            timeout_ms: Some(1_000),
        }
    }

    #[test]
    fn native_encoder_matches_msvc_argument_rules() {
        let mut encoded = Vec::new();
        append_native_argv0(&mut encoded, OsStr::new(r"C:\tool.exe"));
        for argument in ["", "a b", "q\"r", r"trail\", "a b\\"] {
            encoded.push(u16::from(b' '));
            append_native_argument(&mut encoded, OsStr::new(argument));
        }
        assert_eq!(
            String::from_utf16(&encoded).expect("valid fixture UTF-16"),
            "\"C:\\tool.exe\" \"\" \"a b\" q\\\"r trail\\ \"a b\\\\\""
        );
        assert!(finish_native_command_line(vec![u16::from(b'x'); 32_767]).is_err());
    }

    #[test]
    fn native_launch_separates_executable_identity_from_argv0() {
        let resolved = ResolvedProgram {
            absolute: PathBuf::from(r"C:\tools\cargo.exe"),
            executable: PathBuf::from(r"C:\toolchains\rustup.exe"),
            launcher: Launcher::Native,
        };
        let request = fixture_request(vec!["--version"]);

        let launch = LaunchEncoding::new(&resolved, &request).expect("encode native proxy");

        assert_eq!(
            String::from_utf16(&launch.application[..launch.application.len() - 1])
                .expect("valid application UTF-16"),
            r"C:\toolchains\rustup.exe"
        );
        assert_eq!(
            String::from_utf16(&launch.command_line[..launch.command_line.len() - 1])
                .expect("valid command-line UTF-16"),
            r#""C:\tools\cargo.exe" --version"#
        );
    }

    #[test]
    fn batch_encoder_tracks_rust_1_88_regular_argument_policy() {
        let resolved = ResolvedProgram {
            absolute: PathBuf::from(r"C:\repo\probe.cmd"),
            executable: PathBuf::from(r"C:\repo\probe.cmd"),
            launcher: Launcher::CmdCompat,
        };
        let request = fixture_request(vec![
            "%PATH%", "!", "^", "&", "|", "<", ">", "a\"b", "tail\\", "", "界",
        ]);
        let launch = LaunchEncoding::new(&resolved, &request).expect("encode batch corpus");
        let encoded = String::from_utf16(&launch.command_line[..launch.command_line.len() - 1])
            .expect("valid fixture UTF-16");
        assert!(encoded.starts_with("cmd.exe /e:ON /v:OFF /d /c \"\"C:\\repo\\probe.cmd\""));
        assert!(encoded.contains("%%cd:~,%PATH%%cd:~,%"));
        assert!(encoded.ends_with('"'));

        let mut rejected = fixture_request(vec!["line\rbreak"]);
        assert!(LaunchEncoding::new(&resolved, &rejected).is_err());
        rejected.args = vec!["line\nbreak".to_owned()];
        assert!(LaunchEncoding::new(&resolved, &rejected).is_err());
        assert!(finish_batch_command_line(vec![u16::from(b'x'); BATCH_COMMAND_LINE_LIMIT]).is_ok());
        assert!(
            finish_batch_command_line(vec![u16::from(b'x'); BATCH_COMMAND_LINE_LIMIT + 1]).is_err()
        );
    }

    #[test]
    fn every_launcher_transition_failure_uses_raii_cleanup() {
        let fixture = tempfile::tempdir().expect("fixture");
        let executable = std::env::current_exe().expect("test executable");
        let resolved = ResolvedProgram {
            absolute: executable.clone(),
            executable,
            launcher: Launcher::Native,
        };
        for point in [
            FailurePoint::SpawnedSuspended,
            FailurePoint::JobReady,
            FailurePoint::JobAssigned,
            FailurePoint::Running,
        ] {
            let pid_file = fixture.path().join(format!("{point:?}.pid"));
            let mut request = fixture_request(vec![
                "--exact",
                "tools::process::tests::windows_grandchild_child_fixture",
                "--nocapture",
            ]);
            request
                .env
                .insert("CODEXSHIM_PROCESS_FIXTURE".to_owned(), "child".to_owned());
            request.env.insert(
                "CODEXSHIM_PROCESS_PID_FILE".to_owned(),
                pid_file.to_string_lossy().into_owned(),
            );
            FAILURE_POINT.with(|configured| configured.set(Some(point)));
            let result = run(
                &resolved,
                fixture.path(),
                &request,
                std::time::Duration::from_secs(5),
                &tokio_util::sync::CancellationToken::new(),
            );
            FAILURE_POINT.with(|configured| configured.set(None));
            assert!(result.is_err(), "failure point {point:?} was not exercised");
            if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                assert_process_is_gone(pid.trim().parse().expect("pid integer"));
            }
        }
    }

    #[test]
    fn command_evaluation_launchers_and_ps1_are_rejected() {
        let cmd = ResolvedProgram {
            absolute: PathBuf::from(r"C:\tools\safe.exe"),
            executable: PathBuf::from(r"C:\Windows\System32\cmd.exe"),
            launcher: Launcher::Native,
        };
        let cmd_request = fixture_request(vec!["/d", "/c", "echo injected"]);
        assert!(validate_launcher_request(&cmd, &cmd_request).is_err());

        let powershell = ResolvedProgram {
            absolute: PathBuf::from(r"C:\tools\safe.exe"),
            executable: PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            launcher: Launcher::Native,
        };
        for switch in [
            "-Command",
            "-c",
            "-CommandWithArgs",
            "-cwa",
            "-EncodedCommand",
            "-e",
            "-ec",
            "-enc",
            "-command:Get-Process",
            "-encodedcommand=payload",
        ] {
            let powershell_request = fixture_request(vec!["-NoProfile", switch, "Get-Process"]);
            assert!(
                validate_launcher_request(&powershell, &powershell_request).is_err(),
                "{switch} must be rejected"
            );
        }
        assert!(launcher_for(PathBuf::from("script.ps1").as_path()).is_err());
    }

    fn assert_process_is_gone(pid: u32) {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        };

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if !handle.is_null() {
            unsafe {
                CloseHandle(handle);
            }
        }
        assert!(handle.is_null(), "injected failure left child {pid} alive");
    }
}
