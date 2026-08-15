use std::{
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;

use crate::tools::exec::{
    ProcessError,
    capture::{Capture, DRAIN_CHUNK_BYTES, capture_bytes_per_stream},
    spawn::{
        CLEANUP_DEADLINE, DESCENDANT_EXIT_GRACE, EnvironmentPlan, ExecFailure, ExecOutcome,
        ExecPlan, Streams, TERM_GRACE, apply_environment,
    },
};

#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum SetupFailurePoint {
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

pub(crate) fn run(
    plan: &ExecPlan<'_>,
    cancellation: &CancellationToken,
) -> Result<ExecOutcome, ExecFailure> {
    let started = Instant::now();
    let mut lifecycle = spawn_lifecycle(plan)?;

    let mut primary_exit: Option<(String, Instant)> = None;
    let (exit, terminated_descendants) = loop {
        if let Err(error) = lifecycle.poll_io() {
            lifecycle.cleanup()?;
            return Err(ProcessError::Io(error).into());
        }
        if let Some(status) = lifecycle.child_mut().try_wait().map_err(ProcessError::Io)? {
            let (code, detected_at) =
                primary_exit.get_or_insert_with(|| (exit_label(status), Instant::now()));
            if !group_exists(lifecycle.process_group).map_err(ProcessError::Io)? {
                break (code.clone(), false);
            }
            // The primary is gone but the group is not. Descendants get the same grace the
            // Windows job gives them, then the tree goes with the call that owns it.
            if detected_at.elapsed() >= DESCENDANT_EXIT_GRACE {
                break (code.clone(), true);
            }
        }
        if cancellation.is_cancelled() {
            lifecycle.cleanup()?;
            return Err(ProcessError::Cancelled.into());
        }
        if started.elapsed() >= plan.timeout {
            lifecycle.cleanup()?;
            let captures = lifecycle.take_captures()?;
            return Err(ExecFailure::TimedOut {
                duration: started.elapsed(),
                captures,
            });
        }
        lifecycle.wait_io(Duration::from_millis(10))?;
    };

    let captures = if terminated_descendants {
        lifecycle.cleanup()?;
        lifecycle.take_captures()?
    } else {
        lifecycle.finish()?
    };
    Ok(ExecOutcome {
        exit,
        duration: started.elapsed(),
        captures,
    })
}

fn exit_label(status: std::process::ExitStatus) -> String {
    status.code().map_or_else(
        || format!("signal {}", status.signal().unwrap_or_default()),
        |code| code.to_string(),
    )
}

fn spawn_lifecycle(plan: &ExecPlan<'_>) -> Result<Lifecycle, ProcessError> {
    let mut command = Command::new(&plan.resolved.executable);
    command
        .arg0(&plan.resolved.absolute)
        .args(plan.args)
        .current_dir(plan.cwd)
        .stdin(Stdio::piped());
    let merged_reader = configure_output(&mut command, plan.streams)?;
    apply_environment(&mut command, plan.environment);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            arm_parent_death_signal()?;
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
    let readers = if let Some(reader) = merged_reader {
        vec![reader]
    } else {
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
        vec![
            File::from(OwnedFd::from(stdout)),
            File::from(OwnedFd::from(stderr)),
        ]
    };
    let io = UnixIo::new(
        File::from(OwnedFd::from(stdin)),
        readers,
        capture_bytes_per_stream(plan.streams.count()),
        plan.stdin.unwrap_or_default().as_bytes().to_vec(),
    )?;
    #[cfg(test)]
    fail_setup_for_tests(SetupFailurePoint::Io)?;
    lifecycle.install_io(io);
    Ok(lifecycle)
}

/// Returns the parent read end when the topology merges both child streams onto one pipe.
fn configure_output(command: &mut Command, streams: Streams) -> Result<Option<File>, ProcessError> {
    match streams {
        Streams::Separate => {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            Ok(None)
        }
        Streams::Merged => {
            let (read, write) = merged_pipe()?;
            let duplicate = write.try_clone()?;
            command
                .stdout(Stdio::from(write))
                .stderr(Stdio::from(duplicate));
            Ok(Some(File::from(read)))
        }
    }
}

fn merged_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [0_i32; 2];
    // SAFETY: both calls write exactly two descriptors into the provided array.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let created = unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let created = unsafe { libc::pipe(descriptors.as_mut_ptr()) };
    if created == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned from here on.
    let (read, write) = unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        set_close_on_exec(read.as_raw_fd())?;
        set_close_on_exec(write.as_raw_fd())?;
    }
    Ok((read, write))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn set_close_on_exec(file_descriptor: i32) -> io::Result<()> {
    // SAFETY: fcntl only reads and writes flags for the owned pipe descriptor.
    let flags = unsafe { libc::fcntl(file_descriptor, libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: The descriptor remains owned and the existing flags are preserved.
    if unsafe { libc::fcntl(file_descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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

    fn wait_io(&mut self, timeout: Duration) -> Result<(), ProcessError> {
        self.io
            .as_mut()
            .expect("I/O is installed before waiting")
            .wait(timeout)
            .map_err(ProcessError::Io)
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

    fn finish(&mut self) -> Result<Vec<Capture>, ProcessError> {
        let deadline = Instant::now() + CLEANUP_DEADLINE;
        let settlement = self.settle_io(deadline);
        self.completed = true;
        settlement?;
        self.take_captures()
    }

    fn settle_io(&mut self, deadline: Instant) -> Result<(), ProcessError> {
        let io = self
            .io
            .as_mut()
            .expect("I/O is installed before settlement");
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
            io.wait(Duration::from_millis(10))?;
        }
    }

    fn take_captures(&mut self) -> Result<Vec<Capture>, ProcessError> {
        let io = self
            .io
            .as_mut()
            .expect("I/O is installed before completion");
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
    stdin: Option<File>,
    readers: Vec<Option<File>>,
    captures: Vec<Capture>,
    buffers: Vec<Box<[u8]>>,
    input: Vec<u8>,
    input_offset: usize,
    capture_bytes: usize,
}

impl UnixIo {
    fn new(
        stdin: File,
        readers: Vec<File>,
        capture_bytes: usize,
        input: Vec<u8>,
    ) -> io::Result<Self> {
        set_nonblocking(stdin.as_raw_fd())?;
        for reader in &readers {
            set_nonblocking(reader.as_raw_fd())?;
        }
        let captures = (0..readers.len())
            .map(|_| Capture::new(capture_bytes))
            .collect();
        let buffers = (0..readers.len())
            .map(|_| vec![0_u8; DRAIN_CHUNK_BYTES].into_boxed_slice())
            .collect();
        Ok(Self {
            stdin: Some(stdin),
            readers: readers.into_iter().map(Some).collect(),
            captures,
            buffers,
            input,
            input_offset: 0,
            capture_bytes,
        })
    }

    fn poll(&mut self) -> io::Result<()> {
        self.poll_stdin()?;
        let mut result = Ok(());
        for index in 0..self.readers.len() {
            let outcome = Self::poll_capture(
                &mut self.readers[index],
                &mut self.captures[index],
                &mut self.buffers[index],
            );
            if outcome.is_err() && result.is_ok() {
                result = outcome;
            }
        }
        result
    }

    fn wait(&self, timeout: Duration) -> io::Result<()> {
        let mut descriptors =
            Vec::with_capacity(usize::from(self.stdin.is_some()) + self.readers.len());
        if let Some(stdin) = &self.stdin
            && self.input_offset < self.input.len()
        {
            descriptors.push(libc::pollfd {
                fd: stdin.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            });
        }
        descriptors.extend(self.readers.iter().filter_map(|reader| {
            reader.as_ref().map(|reader| libc::pollfd {
                fd: reader.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            })
        }));
        let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: `descriptors` owns a contiguous array for the duration of this call. `poll`
        // ignores the pointer when the descriptor count is zero and provides the bounded wait.
        let result = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                libc::nfds_t::try_from(descriptors.len()).unwrap_or(libc::nfds_t::MAX),
                timeout_ms,
            )
        };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            Ok(())
        } else {
            Err(error)
        }
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

    fn poll_capture(
        pipe: &mut Option<File>,
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
        for reader in &mut self.readers {
            reader.take();
        }
    }

    fn is_settled(&self) -> bool {
        self.stdin.is_none() && self.readers.iter().all(Option::is_none)
    }

    fn take_captures(&mut self) -> Vec<Capture> {
        self.captures
            .iter_mut()
            .map(|capture| std::mem::replace(capture, Capture::new(self.capture_bytes)))
            .collect()
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
            let _ = child.wait()?;
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
            let _ = child.wait()?;
            return Ok(());
        }
        if Instant::now() >= cleanup_deadline {
            return Err(ProcessError::OutcomeUncertain);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub(super) fn signal_group(process_group: i32, signal: i32) -> io::Result<()> {
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

pub(super) fn group_exists(process_group: i32) -> io::Result<bool> {
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

/// Request the kernel to deliver `SIGKILL` when the parent process exits, so an abrupt
/// owner death (SIGKILL, OOM, crash) still reaps the child tree without relying on
/// `Drop`. The `getppid` re-check closes the race where the parent dies between fork
/// and `prctl`: if reparenting already happened the pdeathsig would never fire, so the
/// child must bail out before `exec`.
#[cfg(target_os = "linux")]
fn arm_parent_death_signal() -> io::Result<()> {
    let ppid = unsafe { libc::getppid() };
    // SAFETY: PR_SET_PDEATHSIG with SIGKILL is a valid Linux prctl request.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::getppid() } != ppid {
        return Err(io::Error::other(
            "parent died before parent-death signal was armed",
        ));
    }
    Ok(())
}

/// Spawn a process tree whose lifetime outlives this call, writing both output streams
/// directly to `log`. No pipe, drain thread, or capture is created.
pub(crate) fn spawn_detached(
    plan: &ExecPlan<'_>,
    environment: &EnvironmentPlan,
    log: File,
) -> Result<DetachedTree, ProcessError> {
    let duplicate = log.try_clone()?;
    let mut command = Command::new(&plan.resolved.executable);
    command
        .arg0(&plan.resolved.absolute)
        .args(plan.args)
        .current_dir(plan.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(duplicate));
    apply_environment(&mut command, environment);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            arm_parent_death_signal()?;
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let pid = child.id();
    let Ok(process_group) = i32::try_from(pid) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ProcessError::Io(io::Error::other(
            "child process ID does not fit pid_t",
        )));
    };
    Ok(DetachedTree {
        pid,
        process_group,
        child,
    })
}

pub(crate) struct DetachedTree {
    pid: u32,
    process_group: i32,
    child: Child,
}

impl DetachedTree {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// Fallible on purpose: a failed `kill(-pgroup, 0)` probe says nothing about the tree,
    /// and callers must keep the owner rather than treat the tree as reaped.
    pub(crate) fn is_running(&mut self) -> io::Result<bool> {
        let _ = self.child.try_wait();
        let running = group_exists(self.process_group)?;
        if !running {
            let _ = self.child.wait();
        }
        Ok(running)
    }

    #[cfg(test)]
    pub(crate) fn terminate(&mut self) {
        if terminate(
            self.process_group,
            &mut self.child,
            Instant::now() + CLEANUP_DEADLINE,
        )
        .is_err()
        {
            let _ = signal_group(self.process_group, libc::SIGKILL);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Terminate the process group and confirm it died before `deadline`, sharing one
    /// deadline across every tree a shutdown owns instead of budgeting five seconds each.
    pub(crate) fn terminate_and_wait(&mut self, deadline: Instant) -> Result<(), ProcessError> {
        let outcome = terminate(self.process_group, &mut self.child, deadline);
        if outcome.is_err() {
            let _ = signal_group(self.process_group, libc::SIGKILL);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        outcome
    }
}

/// Mirrors the Windows job's `KILL_ON_JOB_CLOSE`: losing ownership of the tree must not leave
/// it running, including on paths that never reach the orderly shutdown.
impl Drop for DetachedTree {
    fn drop(&mut self) {
        let _ = signal_group(self.process_group, libc::SIGKILL);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
pub(crate) fn active_pipe_workers_for_tests() -> usize {
    0
}

#[cfg(test)]
pub(crate) fn set_setup_failure_for_tests(point: SetupFailurePoint) {
    SETUP_FAILURE.set(Some(point));
    LAST_SPAWNED_PROCESS_GROUP.set(None);
}

#[cfg(test)]
pub(crate) fn take_spawned_process_group_for_tests() -> Option<i32> {
    LAST_SPAWNED_PROCESS_GROUP.take()
}

#[cfg(test)]
pub(crate) fn process_group_exists_for_tests(process_group: i32) -> bool {
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

#[cfg(test)]
mod readiness_tests {
    use super::*;

    #[test]
    fn pipe_readiness_wakes_a_long_poll_immediately() {
        let (read, write) = merged_pipe().expect("pipe");
        let mut io = UnixIo::new(
            File::open("/dev/null").expect("null input"),
            vec![File::from(read)],
            1024,
            Vec::new(),
        )
        .expect("unix I/O");
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let mut writer = File::from(write);
            writer.write_all(b"ready").expect("pipe write");
        });
        let started = Instant::now();

        io.wait(Duration::from_secs(2)).expect("readiness wait");
        assert!(started.elapsed() < Duration::from_millis(500));
        io.poll().expect("drain");
        writer.join().expect("writer");
        io.poll().expect("EOF");
        assert_eq!(io.captures[0].bytes_read, 5);
    }
}
