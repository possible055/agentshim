#[cfg(unix)]
mod platform {
    use std::{
        io::{self, Read, Write},
        os::{
            fd::AsRawFd,
            unix::process::{CommandExt, ExitStatusExt},
        },
        process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use tokio_util::sync::CancellationToken;

    use super::{
        CLEANUP_DEADLINE, Capture, CompletedProcess, DRAIN_CHUNK_BYTES, Path, ProcessError,
        ProcessRequest, ResolvedProgram, TERM_GRACE, TimedOutProcess, ToolOutput,
        apply_environment, render_completed, render_timeout,
    };

    #[cfg(test)]
    #[derive(Clone, Copy, Eq, PartialEq)]
    pub(super) enum SetupFailurePoint {
        Spawn,
        Stdin,
        Stdout,
        Stderr,
        Io,
    }

    #[cfg(test)]
    thread_local! {
        static SETUP_FAILURE: std::cell::Cell<Option<SetupFailurePoint>> = const { std::cell::Cell::new(None) };
        static LAST_SPAWNED_PROCESS_GROUP: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
    }

    pub(super) fn run(
        resolved: &ResolvedProgram,
        cwd: &Path,
        request: &ProcessRequest,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ProcessError> {
        let started = Instant::now();
        let mut lifecycle = spawn_lifecycle(resolved, cwd, request)?;

        let exit = loop {
            if let Err(error) = lifecycle.poll_io() {
                lifecycle.cleanup()?;
                return Err(ProcessError::Io(error));
            }
            if cancellation.is_cancelled() {
                lifecycle.cleanup()?;
                return Err(ProcessError::Cancelled);
            }
            if started.elapsed() >= timeout {
                lifecycle.cleanup()?;
                let (stdout, stderr) = lifecycle.take_captures()?;
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
            if let Some(status) = lifecycle.child_mut().try_wait()? {
                if !group_exists(lifecycle.process_group)? {
                    break if let Some(code) = status.code() {
                        code.to_string()
                    } else {
                        format!("signal {}", status.signal().unwrap_or_default())
                    };
                }
            }
            thread::sleep(Duration::from_millis(10));
        };

        let (stdout, stderr) = lifecycle.finish()?;
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

    fn spawn_lifecycle(
        resolved: &ResolvedProgram,
        cwd: &Path,
        request: &ProcessRequest,
    ) -> Result<Lifecycle, ProcessError> {
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

        let mut lifecycle = Lifecycle::new(command.spawn()?)?;
        record_spawn_for_tests(lifecycle.process_group);
        #[cfg(test)]
        fail_setup_for_tests(SetupFailurePoint::Spawn)?;
        let stdin = lifecycle
            .child_mut()
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("child stdin pipe was not created"))?;
        #[cfg(test)]
        fail_setup_for_tests(SetupFailurePoint::Stdin)?;
        let stdout = lifecycle
            .child_mut()
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("child stdout pipe was not created"))?;
        #[cfg(test)]
        fail_setup_for_tests(SetupFailurePoint::Stdout)?;
        let stderr = lifecycle
            .child_mut()
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("child stderr pipe was not created"))?;
        #[cfg(test)]
        fail_setup_for_tests(SetupFailurePoint::Stderr)?;
        let io = UnixIo::new(
            stdin,
            stdout,
            stderr,
            request.stdin.clone().unwrap_or_default().into_bytes(),
        )?;
        #[cfg(test)]
        fail_setup_for_tests(SetupFailurePoint::Io)?;
        lifecycle.install_io(io);
        Ok(lifecycle)
    }

    struct Lifecycle {
        child: Child,
        process_group: i32,
        io: Option<UnixIo>,
        completed: bool,
    }

    impl Lifecycle {
        fn new(mut child: Child) -> Result<Self, ProcessError> {
            let Ok(process_group) = i32::try_from(child.id()) else {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProcessError::Io(io::Error::other(
                    "child process ID does not fit pid_t",
                )));
            };
            Ok(Self {
                child,
                process_group,
                io: None,
                completed: false,
            })
        }

        fn child_mut(&mut self) -> &mut Child {
            &mut self.child
        }

        fn install_io(&mut self, io: UnixIo) {
            self.io = Some(io);
        }

        fn poll_io(&mut self) -> io::Result<()> {
            self.io
                .as_mut()
                .expect("I/O is installed before polling")
                .poll()
        }

        fn cleanup(&mut self) -> Result<(), ProcessError> {
            let deadline = Instant::now() + CLEANUP_DEADLINE;
            if let Some(io) = self.io.as_mut() {
                io.close_stdin();
            }
            let termination = terminate(self.process_group, &mut self.child, deadline);
            let settlement = self.settle_io(deadline);
            if termination.is_err() {
                self.best_effort_cleanup();
            }
            self.completed = true;
            termination?;
            settlement
        }

        fn finish(&mut self) -> Result<(Capture, Capture), ProcessError> {
            let deadline = Instant::now() + CLEANUP_DEADLINE;
            let settlement = self.settle_io(deadline);
            self.completed = true;
            settlement?;
            self.take_captures()
        }

        fn settle_io(&mut self, deadline: Instant) -> Result<(), ProcessError> {
            let io = self.io.as_mut().expect("I/O is installed before settlement");
            loop {
                if let Err(error) = io.poll() {
                    io.close_all();
                    return Err(ProcessError::Io(error));
                }
                if io.is_settled() {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    io.close_all();
                    return Err(ProcessError::OutcomeUncertain);
                }
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn take_captures(&mut self) -> Result<(Capture, Capture), ProcessError> {
            let io = self.io.as_mut().expect("I/O is installed before completion");
            if !io.is_settled() {
                return Err(ProcessError::OutcomeUncertain);
            }
            Ok(io.take_captures())
        }

        fn best_effort_cleanup(&mut self) {
            if let Some(io) = self.io.as_mut() {
                io.close_all();
            }
            let _ = signal_group(self.process_group, libc::SIGKILL);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    impl Drop for Lifecycle {
        fn drop(&mut self) {
            if !self.completed {
                self.best_effort_cleanup();
            }
        }
    }

    struct UnixIo {
        stdin: Option<ChildStdin>,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
        input: Vec<u8>,
        input_offset: usize,
        stdout_capture: Capture,
        stderr_capture: Capture,
        stdout_buffer: Box<[u8]>,
        stderr_buffer: Box<[u8]>,
    }

    impl UnixIo {
        fn new(
            stdin: ChildStdin,
            stdout: ChildStdout,
            stderr: ChildStderr,
            input: Vec<u8>,
        ) -> io::Result<Self> {
            set_nonblocking(stdin.as_raw_fd())?;
            set_nonblocking(stdout.as_raw_fd())?;
            set_nonblocking(stderr.as_raw_fd())?;
            Ok(Self {
                stdin: Some(stdin),
                stdout: Some(stdout),
                stderr: Some(stderr),
                input,
                input_offset: 0,
                stdout_capture: Capture::new(),
                stderr_capture: Capture::new(),
                stdout_buffer: vec![0_u8; DRAIN_CHUNK_BYTES].into_boxed_slice(),
                stderr_buffer: vec![0_u8; DRAIN_CHUNK_BYTES].into_boxed_slice(),
            })
        }

        fn poll(&mut self) -> io::Result<()> {
            self.poll_stdin()?;
            Self::poll_capture(
                &mut self.stdout,
                &mut self.stdout_capture,
                &mut self.stdout_buffer,
            )?;
            Self::poll_capture(
                &mut self.stderr,
                &mut self.stderr_capture,
                &mut self.stderr_buffer,
            )
        }

        fn poll_stdin(&mut self) -> io::Result<()> {
            let Some(stdin) = self.stdin.as_mut() else {
                return Ok(());
            };
            if self.input_offset == self.input.len() {
                self.stdin.take();
                return Ok(());
            }
            match stdin.write(&self.input[self.input_offset..]) {
                Ok(0) => {
                    self.stdin.take();
                    Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write process stdin",
                    ))
                }
                Ok(written) => {
                    self.input_offset += written;
                    if self.input_offset == self.input.len() {
                        self.stdin.take();
                    }
                    Ok(())
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(()),
                Err(error) => {
                    self.stdin.take();
                    Err(error)
                }
            }
        }

        fn poll_capture<R: Read>(
            pipe: &mut Option<R>,
            capture: &mut Capture,
            buffer: &mut [u8],
        ) -> io::Result<()> {
            let Some(reader) = pipe.as_mut() else {
                return Ok(());
            };
            loop {
                match reader.read(buffer) {
                    Ok(0) => {
                        pipe.take();
                        return Ok(());
                    }
                    Ok(count) => capture.push(&buffer[..count]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        pipe.take();
                        return Err(error);
                    }
                }
            }
        }

        fn close_stdin(&mut self) {
            self.stdin.take();
        }

        fn close_all(&mut self) {
            self.stdin.take();
            self.stdout.take();
            self.stderr.take();
        }

        fn is_settled(&self) -> bool {
            self.stdin.is_none() && self.stdout.is_none() && self.stderr.is_none()
        }

        fn take_captures(&mut self) -> (Capture, Capture) {
            (
                std::mem::replace(&mut self.stdout_capture, Capture::new()),
                std::mem::replace(&mut self.stderr_capture, Capture::new()),
            )
        }
    }

    fn set_nonblocking(file_descriptor: i32) -> io::Result<()> {
        // SAFETY: fcntl only reads flags for the owned pipe descriptor.
        let flags = unsafe { libc::fcntl(file_descriptor, libc::F_GETFL) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: The descriptor remains owned and the existing flags are preserved.
        if unsafe { libc::fcntl(file_descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn terminate(
        process_group: i32,
        child: &mut Child,
        cleanup_deadline: Instant,
    ) -> Result<(), ProcessError> {
        signal_group(process_group, libc::SIGTERM)?;
        let grace_deadline = (Instant::now() + TERM_GRACE).min(cleanup_deadline);
        loop {
            let _ = child.try_wait()?;
            if !group_exists(process_group)? {
                return Ok(());
            }
            if Instant::now() >= grace_deadline {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        signal_group(process_group, libc::SIGKILL)?;
        loop {
            let _ = child.try_wait()?;
            if !group_exists(process_group)? {
                return Ok(());
            }
            if Instant::now() >= cleanup_deadline {
                return Err(ProcessError::OutcomeUncertain);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn signal_group(process_group: i32, signal: i32) -> io::Result<()> {
        // SAFETY: A negative PID targets only the child-owned process group.
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
        // SAFETY: Signal zero performs a read-only existence check for the process group.
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

    #[cfg(test)]
    pub(super) fn active_pipe_workers_for_tests() -> usize {
        0
    }

    #[cfg(test)]
    pub(super) fn set_setup_failure_for_tests(point: SetupFailurePoint) {
        SETUP_FAILURE.set(Some(point));
        LAST_SPAWNED_PROCESS_GROUP.set(None);
    }

    #[cfg(test)]
    pub(super) fn take_spawned_process_group_for_tests() -> Option<i32> {
        LAST_SPAWNED_PROCESS_GROUP.take()
    }

    #[cfg(test)]
    pub(super) fn process_group_exists_for_tests(process_group: i32) -> bool {
        group_exists(process_group).unwrap_or(true)
    }

    #[cfg(test)]
    fn record_spawn_for_tests(process_group: i32) {
        LAST_SPAWNED_PROCESS_GROUP.set(Some(process_group));
    }

    #[cfg(not(test))]
    fn record_spawn_for_tests(_process_group: i32) {}

    #[cfg(test)]
    fn fail_setup_for_tests(point: SetupFailurePoint) -> io::Result<()> {
        if SETUP_FAILURE.get() == Some(point) {
            SETUP_FAILURE.set(None);
            return Err(io::Error::other("injected Unix lifecycle setup failure"));
        }
        Ok(())
    }
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

include!("tests.rs");
