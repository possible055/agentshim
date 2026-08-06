#[cfg(unix)]
mod platform {
    use std::{
        io,
        os::unix::process::{CommandExt, ExitStatusExt},
        process::{Command, Stdio},
        thread,
    };

    use super::{
        CLEANUP_DEADLINE, Capture, CompletedProcess, Path, ProcessError, ProcessRequest,
        ResolvedProgram, TERM_GRACE, ThreadCompletion, TimedOutProcess, apply_environment, drain,
        render_completed, render_timeout, spawn_monitored, write_stdin, ToolOutput,
    };
    use std::sync::{Arc, atomic::Ordering};
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    pub(super) fn run(
        resolved: &ResolvedProgram,
        cwd: &Path,
        request: &ProcessRequest,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ProcessError> {
        let started = Instant::now();
        let mut command = Command::new(&resolved.executable);
        command
            .arg0(&resolved.absolute)
            .args(&request.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_environment(&mut command, request);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let process_group = i32::try_from(child.id()).map_err(|_| {
            ProcessError::Io(io::Error::other("child process ID does not fit pid_t"))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("child stdin pipe was not created"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("child stdout pipe was not created"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("child stderr pipe was not created"))?;
        let input = request.stdin.clone();
        let io_failed = Arc::new(super::AtomicBool::new(false));
        let completion = ThreadCompletion::new();
        let stdin_thread = spawn_monitored(Arc::clone(&io_failed), completion.clone(), move || {
            write_stdin(stdin, input.as_deref())
        });
        let stdout_thread =
            spawn_monitored(Arc::clone(&io_failed), completion.clone(), move || {
                drain(stdout)
            });
        let stderr_thread =
            spawn_monitored(Arc::clone(&io_failed), completion.clone(), move || {
                drain(stderr)
            });

        let exit = loop {
            if io_failed.load(Ordering::Acquire) {
                terminate(process_group, &mut child)?;
                let (stdin_result, _, _) =
                    settle_threads(&completion, stdin_thread, stdout_thread, stderr_thread)?;
                stdin_result?;
                return Err(ProcessError::Io(io::Error::other(
                    "process I/O task failed without an error",
                )));
            }
            if cancellation.is_cancelled() {
                terminate(process_group, &mut child)?;
                let (stdin_result, _, _) =
                    settle_threads(&completion, stdin_thread, stdout_thread, stderr_thread)?;
                let _ = stdin_result;
                return Err(ProcessError::Cancelled);
            }
            if started.elapsed() >= timeout {
                terminate(process_group, &mut child)?;
                let (stdin_result, stdout, stderr) =
                    settle_threads(&completion, stdin_thread, stdout_thread, stderr_thread)?;
                let _ = stdin_result;
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
                return Err(ProcessError::Timeout {
                    timeout_ms,
                    report: timeout.text,
                    details: Box::new(timeout.details),
                });
            }
            if let Some(status) = child.try_wait()? {
                if !group_exists(process_group)? {
                    break if let Some(code) = status.code() {
                        code.to_string()
                    } else {
                        format!("signal {}", status.signal().unwrap_or_default())
                    };
                }
            }
            thread::sleep(Duration::from_millis(10));
        };

        finish_completed(
            resolved,
            cwd,
            exit,
            started,
            (completion, stdin_thread, stdout_thread, stderr_thread),
            cancellation,
        )
    }

    type PendingIo = (
        ThreadCompletion,
        thread::JoinHandle<io::Result<()>>,
        thread::JoinHandle<io::Result<Capture>>,
        thread::JoinHandle<io::Result<Capture>>,
    );

    fn finish_completed(
        resolved: &ResolvedProgram,
        cwd: &Path,
        exit: String,
        started: Instant,
        (completion, stdin, stdout, stderr): PendingIo,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ProcessError> {
        let (stdin_result, stdout, stderr) = settle_threads(&completion, stdin, stdout, stderr)?;
        stdin_result?;
        render_completed(
            &CompletedProcess {
                resolved: resolved.clone(),
                cwd: cwd.to_owned(),
                exit,
                duration: started.elapsed(),
                stdout,
                stderr,
            },
            cancellation,
        )
    }

    type ThreadResults = (io::Result<()>, Capture, Capture);

    fn settle_threads(
        completion: &ThreadCompletion,
        stdin: thread::JoinHandle<io::Result<()>>,
        stdout: thread::JoinHandle<io::Result<Capture>>,
        stderr: thread::JoinHandle<io::Result<Capture>>,
    ) -> Result<ThreadResults, ProcessError> {
        if !completion.wait_for(3, CLEANUP_DEADLINE) {
            return Err(ProcessError::OutcomeUncertain);
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

    fn terminate(process_group: i32, child: &mut std::process::Child) -> Result<(), ProcessError> {
        signal_group(process_group, libc::SIGTERM)?;
        let grace = Instant::now();
        while grace.elapsed() < TERM_GRACE {
            let _ = child.try_wait()?;
            if !group_exists(process_group)? {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        signal_group(process_group, libc::SIGKILL)?;
        let cleanup = Instant::now();
        while cleanup.elapsed() < CLEANUP_DEADLINE {
            let _ = child.try_wait()?;
            if !group_exists(process_group)? {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(ProcessError::OutcomeUncertain)
    }

    fn signal_group(process_group: i32, signal: i32) -> io::Result<()> {
        let result = unsafe { libc::kill(-process_group, signal) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn group_exists(process_group: i32) -> io::Result<bool> {
        let result = unsafe { libc::kill(-process_group, 0) };
        if result == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Ok(true),
            _ => Err(error),
        }
    }
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

include!("tests.rs");
