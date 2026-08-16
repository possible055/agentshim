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
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Globalization::{CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN, CompareStringOrdinal},
    Security::SECURITY_ATTRIBUTES,
    System::{
        IO::{CancelSynchronousIo, CreateIoCompletionPort, GetQueuedCompletionStatus},
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
            DeleteProcThreadAttributeList, GetExitCodeProcess, InitializeProcThreadAttributeList,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, ResumeThread,
            STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
            WaitForSingleObject,
        },
    },
};

#[cfg(test)]
use super::runner::{FailurePoint, inject_failure};
use crate::tools::exec::{
    ProcessError,
    capture::Capture,
    resolve::{Launcher, ResolvedProgram},
    spawn::{
        CLEANUP_DEADLINE, EnvironmentPlan, IO_CANCELLATION_DEADLINE, Streams, ThreadCompletion,
    },
};

const NATIVE_COMMAND_LINE_LIMIT: usize = 32_767;
pub const BATCH_COMMAND_LINE_LIMIT: usize = 8_191;
pub const TERMINATION_EXIT_CODE: u32 = 0xC0DE_CACE;

pub struct PreparedStdio {
    pub stdin: Pipe,
    pub outputs: Vec<Pipe>,
    pub attributes: AttributeList,
    pub startup: STARTUPINFOEXW,
}

pub fn prepare_stdio(streams: Streams) -> io::Result<PreparedStdio> {
    let stdin = Pipe::stdin()?;
    let outputs = (0..streams.count())
        .map(|_| Pipe::stdout())
        .collect::<io::Result<Vec<_>>>()?;
    // A merged topology points both child output handles at the same pipe, so the parent sees
    // one interleaved stream in pipe-write order.
    let error_handle = outputs
        .last()
        .expect("at least one output pipe is created")
        .child
        .raw();
    let standard = [stdin.child.raw(), outputs[0].child.raw(), error_handle];
    let mut inherited = Vec::with_capacity(standard.len());
    for handle in standard {
        if !inherited.contains(&handle) {
            inherited.push(handle);
        }
    }
    let attributes = AttributeList::new(&inherited)?;
    let startup = startup_info(standard, &attributes)?;
    Ok(PreparedStdio {
        stdin,
        outputs,
        attributes,
        startup,
    })
}

pub fn startup_info(
    inherited: [HANDLE; 3],
    attributes: &AttributeList,
) -> io::Result<STARTUPINFOEXW> {
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

type ThreadResults = (io::Result<()>, Vec<Capture>);

pub fn settle_threads(
    completion: &ThreadCompletion,
    stdin: Option<thread::JoinHandle<io::Result<()>>>,
    drains: Vec<thread::JoinHandle<io::Result<Capture>>>,
) -> Result<ThreadResults, ProcessError> {
    settle_threads_with_deadlines(
        completion,
        stdin,
        drains,
        CLEANUP_DEADLINE,
        IO_CANCELLATION_DEADLINE,
    )
}

pub fn settle_threads_with_deadlines(
    completion: &ThreadCompletion,
    stdin: Option<thread::JoinHandle<io::Result<()>>>,
    drains: Vec<thread::JoinHandle<io::Result<Capture>>>,
    settlement_deadline: Duration,
    cancellation_deadline: Duration,
) -> Result<ThreadResults, ProcessError> {
    let completion_target = drains.len() + usize::from(stdin.is_some());
    if !completion.wait_for(completion_target, settlement_deadline) {
        if let Some(stdin) = &stdin {
            cancel_thread_io(stdin);
        }
        for drain in &drains {
            cancel_thread_io(drain);
        }
        if !completion.wait_for(completion_target, cancellation_deadline) {
            return Err(ProcessError::OutcomeUncertain);
        }
    }
    let stdin = stdin.map_or(Ok(()), |stdin| {
        stdin
            .join()
            .map_err(|_| io::Error::other("stdin writer panicked"))?
    });
    let mut captures = Vec::with_capacity(drains.len());
    for handle in drains {
        captures.push(
            handle
                .join()
                .map_err(|_| io::Error::other("output drainer panicked"))??,
        );
    }
    Ok((stdin, captures))
}

fn cancel_thread_io<T>(thread: &thread::JoinHandle<T>) {
    unsafe {
        CancelSynchronousIo(thread.as_raw_handle());
    }
}

pub struct LaunchEncoding {
    pub application: Vec<u16>,
    pub command_line: Vec<u16>,
}

impl LaunchEncoding {
    pub fn new(resolved: &ResolvedProgram, args: &[String]) -> Result<Self, ProcessError> {
        match resolved.launcher {
            Launcher::Native => {
                let application = nul_terminated(resolved.executable.as_os_str());
                let mut command_line = Vec::new();
                append_native_argv0(&mut command_line, resolved.absolute.as_os_str());
                for argument in args {
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
                append_batch_path(&mut command_line, &script)?;
                command_line.push(u16::from(b'"'));
                for argument in args {
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

pub fn finish_native_command_line(mut encoded: Vec<u16>) -> Result<Vec<u16>, ProcessError> {
    if encoded.len().saturating_add(1) > NATIVE_COMMAND_LINE_LIMIT {
        return Err(ProcessError::Validation(format!(
            "encoded command line exceeds the {NATIVE_COMMAND_LINE_LIMIT} UTF-16 code-unit limit"
        )));
    }
    encoded.push(0);
    Ok(encoded)
}

pub fn finish_batch_command_line(mut encoded: Vec<u16>) -> Result<Vec<u16>, ProcessError> {
    if encoded.len() > BATCH_COMMAND_LINE_LIMIT {
        return Err(ProcessError::Validation(format!(
            "encoded batch command line exceeds the {BATCH_COMMAND_LINE_LIMIT} UTF-16 code-unit limit"
        )));
    }
    encoded.push(0);
    Ok(encoded)
}

pub fn append_native_argument(output: &mut Vec<u16>, argument: &OsStr) {
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

pub fn append_native_argv0(output: &mut Vec<u16>, argument: &OsStr) {
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

fn append_batch_path(output: &mut Vec<u16>, path: &[u16]) -> Result<(), ProcessError> {
    if path.iter().any(|unit| {
        matches!(
            *unit,
            unit if unit == u16::from(b'\0')
                || unit == u16::from(b'\r')
                || unit == u16::from(b'\n')
        )
    }) {
        return Err(ProcessError::Validation(
            "cmd-compat script path must not contain NUL, CR, or LF".to_owned(),
        ));
    }
    for unit in path {
        if *unit == u16::from(b'%') {
            append_ascii(output, "%%cd:~,");
        }
        output.push(*unit);
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

pub fn create_process_cwd(path: &Path) -> Result<Vec<u16>, ProcessError> {
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

pub fn environment_block(plan: &EnvironmentPlan) -> Vec<u16> {
    let mut variables = plan.base.as_ref().map_or_else(
        || std::env::vars_os().collect::<Vec<_>>(),
        |base| {
            base.iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value)))
                .collect()
        },
    );
    for (key, value) in &plan.injected {
        set_environment(&mut variables, OsStr::new(key), OsStr::new(value));
    }
    for key in &plan.removed {
        variables.retain(|(existing, _)| !environment_key_equal(existing, OsStr::new(key)));
    }
    for (key, value) in &plan.overrides {
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

pub struct Pipe {
    pub parent: OwnedHandle,
    pub child: OwnedHandle,
}

impl Pipe {
    pub fn stdin() -> io::Result<Self> {
        Self::create(true)
    }

    pub fn stdout() -> io::Result<Self> {
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

pub struct AttributeList {
    storage: Vec<usize>,
    handles: Box<[HANDLE]>,
}

impl AttributeList {
    pub fn new(handles: &[HANDLE]) -> io::Result<Self> {
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
        let handles = handles.to_vec().into_boxed_slice();
        let list = Self { storage, handles };
        if unsafe {
            UpdateProcThreadAttribute(
                list.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                list.handles.as_ptr().cast::<c_void>(),
                mem::size_of_val(list.handles.as_ref()),
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

pub struct Lifecycle {
    pid: u32,
    process: Option<OwnedHandle>,
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
    pub fn new(info: PROCESS_INFORMATION) -> io::Result<Self> {
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
            pid: info.dwProcessId,
            process: Some(process),
            thread: Some(thread),
            job: None,
            completion: None,
            state: LifecycleState::SpawnedSuspended,
        })
    }

    pub fn primary_pid(&self) -> u32 {
        self.pid
    }

    pub fn install_job(&mut self) -> io::Result<()> {
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
            CompletionKey: job.raw(),
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
                self.process
                    .as_ref()
                    .expect("primary process handle available")
                    .raw(),
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

    pub fn resume(&mut self) -> io::Result<()> {
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

    pub fn primary_exit_code(&self) -> io::Result<Option<u32>> {
        let process = self
            .process
            .as_ref()
            .ok_or_else(|| io::Error::other("primary process handle is unavailable"))?;
        let wait = unsafe { WaitForSingleObject(process.raw(), 0) };
        match wait {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => return Ok(None),
            _ => return Err(io::Error::last_os_error()),
        }
        let mut code = 0_u32;
        if unsafe { GetExitCodeProcess(process.raw(), &raw mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(code))
    }

    pub fn active_processes(&self) -> io::Result<u32> {
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

    pub fn poll_completion_hint(&self, wait_ms: u32) {
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
                wait_ms,
            );
        }
    }

    pub fn terminate_and_wait(&mut self) -> Result<(), ProcessError> {
        let job = self
            .job
            .as_ref()
            .ok_or_else(|| io::Error::other("job handle is unavailable"))?;
        if unsafe { TerminateJobObject(job.raw(), TERMINATION_EXIT_CODE) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let started = Instant::now();
        while started.elapsed() < CLEANUP_DEADLINE {
            self.poll_completion_hint(10);
            if self.primary_exit_code()?.is_some() && self.active_processes()? == 0 {
                return Ok(());
            }
        }
        Err(ProcessError::OutcomeUncertain)
    }

    pub fn finish(&mut self) {
        debug_assert_eq!(self.state, LifecycleState::Running);
        self.state = LifecycleState::Complete;
    }

    /// Hand the job over to a longer-lived owner. The lifecycle stops managing the tree, so
    /// dropping it no longer terminates the processes inside the job.
    pub fn release_detached_handles(&mut self) -> (OwnedHandle, OwnedHandle) {
        let job = self.job.take().expect("job installed before release");
        let process = self
            .process
            .take()
            .expect("primary process handle available before release");
        self.state = LifecycleState::Complete;
        (job, process)
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
            } else if let Some(process) = &self.process {
                TerminateProcess(process.raw(), TERMINATION_EXIT_CODE);
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

pub struct OwnedHandle(HANDLE);

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

    pub fn raw(&self) -> HANDLE {
        self.0
    }

    pub fn into_file(self) -> File {
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
