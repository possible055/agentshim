use std::{
    ffi::c_void,
    fs::File,
    io,
    mem::size_of,
    os::windows::io::AsRawHandle,
    ptr::{null, null_mut},
};

#[cfg(test)]
use std::cell::RefCell;

#[cfg(test)]
thread_local! {
    static AFTER_PRIMARY_OBSERVATION: RefCell<Option<Box<dyn FnOnce()>>> = const {
        RefCell::new(None)
    };
}

use windows_sys::Win32::{
    Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation},
    System::{
        JobObjects::{
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
            QueryInformationJobObject, TerminateJobObject,
        },
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, PROCESS_INFORMATION,
            WaitForSingleObject,
        },
    },
};

use crate::tools::exec::{
    ProcessError,
    spawn::{EnvironmentPlan, ExecPlan},
};

use super::platform::{
    AttributeList, LaunchEncoding, Lifecycle, OwnedHandle, Pipe, TERMINATION_EXIT_CODE,
    create_process_cwd, environment_block, startup_info,
};

pub fn spawn_detached_capture(
    plan: &ExecPlan<'_>,
    environment: &EnvironmentPlan,
) -> Result<(DetachedTree, File), ProcessError> {
    let launch = LaunchEncoding::new(plan.resolved, plan.args)?;
    let block = environment_block(environment);
    let cwd_wide = create_process_cwd(plan.cwd)?;
    let null_input = File::open("NUL")?;
    let output = Pipe::stdout()?;
    let input_handle = null_input.as_raw_handle() as HANDLE;
    let output_handle = output.child.raw();
    // Safety: the handle is owned by this function and the call only toggles
    // its inherit flag for the child that is about to be created.
    if unsafe { SetHandleInformation(input_handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    let inherited = [input_handle, output_handle];
    let attributes = AttributeList::new(&inherited)?;
    let startup = startup_info([input_handle, output_handle, output_handle], &attributes)?;
    let mut command_line = launch.command_line;
    let mut process_info = PROCESS_INFORMATION::default();
    // Safety: the application and command-line buffers are NUL-terminated wide
    // strings owned by `launch`, the environment block stays alive for the
    // call, the startup-info pointer matches `EXTENDED_STARTUPINFO_PRESENT`,
    // and `process_info` is a valid out pointer.
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
    let (job, process) = lifecycle.release_detached_handles();
    let reader = output.parent.into_file();
    drop(output.child);
    drop(null_input);
    Ok((
        DetachedTree {
            pid,
            job,
            process,
            primary_exit: None,
        },
        reader,
    ))
}

/// Spawn a process tree whose lifetime outlives this call, writing both output streams
/// directly to `log`. The job handle keeps `KILL_ON_JOB_CLOSE`, so the tree dies with the
/// server instance that owns the returned value.
pub fn spawn_detached(
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
        // Safety: both handles are owned by this function and the call only
        // toggles their inherit flag for the child that is about to be created.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    let inherited = [input_handle, log_handle];
    let attributes = AttributeList::new(&inherited)?;
    let startup = startup_info([input_handle, log_handle, log_handle], &attributes)?;

    let mut command_line = launch.command_line;
    let mut process_info = PROCESS_INFORMATION::default();
    // Safety: the application and command-line buffers are NUL-terminated wide
    // strings owned by `launch`, the environment block stays alive for the
    // call, the startup-info pointer matches `EXTENDED_STARTUPINFO_PRESENT`,
    // and `process_info` is a valid out pointer.
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
    let (job, process) = lifecycle.release_detached_handles();
    drop(log);
    drop(null_input);
    Ok(DetachedTree {
        pid,
        job,
        process,
        primary_exit: None,
    })
}

pub struct DetachedTree {
    pid: u32,
    job: OwnedHandle,
    process: OwnedHandle,
    primary_exit: Option<String>,
}

impl DetachedTree {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Fallible on purpose: a failed `QueryInformationJobObject` says nothing about the
    /// tree, and callers must keep the job owner rather than treat the tree as reaped.
    pub fn observe(&mut self) -> io::Result<super::super::DetachedObservation> {
        self.refresh_primary_exit()?;
        #[cfg(test)]
        run_after_primary_observation_hook();
        let tree_running = job_active_processes(self.job.raw())? > 0;
        if !tree_running && self.primary_exit.is_none() {
            self.refresh_primary_exit()?;
            if self.primary_exit.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "job is empty while the primary process handle is not signaled",
                ));
            }
        }
        Ok(super::super::DetachedObservation {
            tree_running,
            primary_exit: self.primary_exit.clone(),
        })
    }

    fn refresh_primary_exit(&mut self) -> io::Result<()> {
        if self.primary_exit.is_some() {
            return Ok(());
        }
        // Safety: the process handle is owned by `self`; a zero timeout makes
        // the call a non-blocking poll with no side effects.
        let wait = unsafe { WaitForSingleObject(self.process.raw(), 0) };
        match wait {
            windows_sys::Win32::Foundation::WAIT_OBJECT_0 => {
                let mut code = 0_u32;
                // Safety: the process handle is owned by `self` and `code` is a
                // valid out parameter for the duration of the call.
                if unsafe { GetExitCodeProcess(self.process.raw(), &raw mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                self.primary_exit = Some(code.to_string());
                Ok(())
            }
            windows_sys::Win32::Foundation::WAIT_TIMEOUT => Ok(()),
            windows_sys::Win32::Foundation::WAIT_FAILED => Err(io::Error::last_os_error()),
            other => Err(io::Error::other(format!(
                "unexpected primary process wait result: {other:#x}"
            ))),
        }
    }

    /// Terminate the job and confirm active processes reached zero before `deadline`.
    /// Termination itself is forceful; the wait is what makes the outcome verifiable, so a
    /// failed accounting query is reported instead of guessed away. Polling backs off
    /// instead of busy-looping on the accounting call.
    pub fn terminate_and_wait(&mut self, deadline: std::time::Instant) -> Result<(), ProcessError> {
        // Safety: the job handle is owned by `self` and the call terminates
        // exactly the process tree assigned to that job.
        if unsafe { TerminateJobObject(self.job.raw(), TERMINATION_EXIT_CODE) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut delay = std::time::Duration::from_millis(5);
        while std::time::Instant::now() < deadline {
            match job_active_processes(self.job.raw()) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        target: "agentshim",
                        event = "detached_liveness_degraded",
                        phase = "execution",
                        outcome = "degraded",
                        error_class = "io",
                        io_kind = ?error.kind(),
                        pid = self.pid
                    );
                    return Err(ProcessError::OutcomeUncertain);
                }
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(std::time::Duration::from_millis(50));
        }
        Err(ProcessError::OutcomeUncertain)
    }
}

#[cfg(test)]
fn run_after_primary_observation_hook() {
    AFTER_PRIMARY_OBSERVATION.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
pub fn set_after_primary_observation_hook_for_tests(hook: impl FnOnce() + 'static) {
    AFTER_PRIMARY_OBSERVATION.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

fn job_active_processes(job: HANDLE) -> io::Result<u32> {
    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // Safety: the job handle is valid for the call and the accounting struct
    // with its exact size is a valid out parameter.
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
