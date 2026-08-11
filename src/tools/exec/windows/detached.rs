use std::{
    ffi::c_void,
    fs::File,
    io,
    mem::size_of,
    os::windows::io::AsRawHandle,
    ptr::{null, null_mut},
};

use windows_sys::Win32::{
    Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation},
    System::{
        JobObjects::{
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
            QueryInformationJobObject, TerminateJobObject,
        },
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
        },
    },
};

use super::{
    super::{
        ProcessError,
        spawn::{EnvironmentPlan, ExecPlan},
    },
    platform::{
        AttributeList, LaunchEncoding, Lifecycle, OwnedHandle, TERMINATION_EXIT_CODE,
        create_process_cwd, environment_block, startup_info,
    },
};

/// Spawn a process tree whose lifetime outlives this call, writing both output streams
/// directly to `log`. The job handle keeps `KILL_ON_JOB_CLOSE`, so the tree dies with the
/// server instance that owns the returned value.
pub(crate) fn spawn_detached(
    plan: &ExecPlan<'_>,
    environment: &EnvironmentPlan,
    log: File,
) -> Result<DetachedTree, ProcessError> {
    let launch = LaunchEncoding::new(plan.resolved, plan.args)?;
    let block = environment_block(environment);
    let cwd_wide = create_process_cwd(plan.cwd)?;
    let null_input = File::open("NUL")?;
    let log_handle = log.as_raw_handle() as HANDLE;
    let input_handle = null_input.as_raw_handle() as HANDLE;
    for handle in [input_handle, log_handle] {
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    let inherited = [input_handle, log_handle];
    let attributes = AttributeList::new(&inherited)?;
    let startup = startup_info([input_handle, log_handle, log_handle], &attributes)?;

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
            block.as_ptr().cast::<c_void>(),
            cwd_wide.as_ptr(),
            &raw const startup.StartupInfo,
            &raw mut process_info,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error().into());
    }
    drop(attributes);
    let mut lifecycle = Lifecycle::new(process_info)?;
    lifecycle.install_job()?;
    lifecycle.resume()?;
    let pid = process_info.dwProcessId;
    let job = lifecycle.release_job();
    drop(log);
    drop(null_input);
    Ok(DetachedTree { pid, job })
}

pub(crate) struct DetachedTree {
    pid: u32,
    job: OwnedHandle,
}

impl DetachedTree {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn is_running(&mut self) -> bool {
        job_active_processes(self.job.raw()).unwrap_or(0) > 0
    }

    pub(crate) fn terminate(&mut self) {
        unsafe {
            TerminateJobObject(self.job.raw(), TERMINATION_EXIT_CODE);
        }
    }
}

fn job_active_processes(job: HANDLE) -> io::Result<u32> {
    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    if unsafe {
        QueryInformationJobObject(
            job,
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
