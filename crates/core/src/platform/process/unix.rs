use std::{
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;

use crate::platform::process::{
    ProcessError,
    capture::{Capture, DRAIN_CHUNK_BYTES, capture_bytes_per_stream},
    spawn::{
        CLEANUP_DEADLINE, CaptureSink, DESCENDANT_EXIT_GRACE, EnvironmentPlan, ExecFailure,
        ExecOutcome, ExecPlan, Streams, TERM_GRACE, apply_environment,
    },
};

#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum SetupFailurePoint {
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

#[cfg(all(test, target_os = "linux"))]
static FORCE_POLL_FALLBACK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn run(
    plan: &ExecPlan<'_>,
    cancellation: &CancellationToken,
    capture_sink: Option<&Arc<dyn CaptureSink>>,
) -> Result<ExecOutcome, ExecFailure> {
    let started = Instant::now();
    // UnixIo keeps the sink alive across the poll loop, so it owns an Arc clone.
    let mut lifecycle = spawn_lifecycle(plan, capture_sink.cloned())?;

    let mut primary_exit: Option<(String, Instant)> = None;
    let (exit, terminated_descendants) = loop {
        if let Err(error) = lifecycle.poll_io() {
            lifecycle.cleanup()?;
            return Err(ProcessError::from(error).into());
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
        lifecycle.wait_io(Duration::from_millis(10), primary_exit.is_some())?;
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

pub fn spawn_detached_capture(
    plan: &ExecPlan<'_>,
    environment: &EnvironmentPlan,
) -> Result<(DetachedTree, File), ProcessError> {
    let (read, write) = merged_pipe()?;
    let duplicate = write.try_clone()?;
    let mut command = Command::new(&plan.resolved.executable);
    command
        .arg0(&plan.resolved.absolute)
        .args(plan.args)
        .current_dir(plan.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(write))
        .stderr(Stdio::from(duplicate));
    apply_environment(&mut command, environment);
    // Safety: `pre_exec` runs the closure between `fork` and `exec`, so it may
    // only call async-signal-safe functions; `setsid`, `prctl`, and `getppid`
    // are, and the closure performs no allocation.
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
    let child = command.spawn()?;
    let pid = i32::try_from(child.id())
        .map_err(|_| ProcessError::Io(io::Error::other("child process ID does not fit pid_t")))?;
    record_spawn_for_tests(pid);
    Ok((
        DetachedTree {
            pid: child.id(),
            process_group: pid,
            child,
            primary_exit: None,
        },
        File::from(read),
    ))
}

fn exit_label(status: std::process::ExitStatus) -> String {
    status.code().map_or_else(
        || format!("signal {}", status.signal().unwrap_or_default()),
        |code| code.to_string(),
    )
}

fn take_pipe<P>(slot: &mut Option<P>, name: &str) -> io::Result<P> {
    slot.take()
        .ok_or_else(|| io::Error::other(format!("child {name} pipe was not created")))
}

fn spawn_lifecycle(
    plan: &ExecPlan<'_>,
    capture_sink: Option<Arc<dyn CaptureSink>>,
) -> Result<Lifecycle, ProcessError> {
    let input = plan.stdin.filter(|input| !input.is_empty());
    let mut command = Command::new(&plan.resolved.executable);
    command
        .arg0(&plan.resolved.absolute)
        .args(plan.args)
        .current_dir(plan.cwd)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let merged_reader = configure_output(&mut command, plan.streams)?;
    apply_environment(&mut command, plan.environment);
    // Safety: `pre_exec` runs the closure between `fork` and `exec`, so it may
    // only call async-signal-safe functions; `setsid`, `prctl`, and `getppid`
    // are, and the closure performs no allocation.
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
    let stdin = if input.is_some() {
        let stdin = File::from(OwnedFd::from(take_pipe(
            &mut lifecycle.child_mut().stdin,
            "stdin",
        )?));
        #[cfg(test)]
        fail_setup_for_tests(SetupFailurePoint::Stdin)?;
        Some(stdin)
    } else {
        None
    };
    let readers = if let Some(reader) = merged_reader {
        vec![reader]
    } else {
        let stdout = take_pipe(&mut lifecycle.child_mut().stdout, "stdout")?;
        #[cfg(test)]
        fail_setup_for_tests(SetupFailurePoint::Stdout)?;
        let stderr = take_pipe(&mut lifecycle.child_mut().stderr, "stderr")?;
        #[cfg(test)]
        fail_setup_for_tests(SetupFailurePoint::Stderr)?;
        vec![
            File::from(OwnedFd::from(stdout)),
            File::from(OwnedFd::from(stderr)),
        ]
    };
    let io = UnixIo::new(
        stdin,
        readers,
        capture_bytes_per_stream(plan.streams.count(), plan.capture_page_bytes),
        input.unwrap_or_default().as_bytes().to_vec(),
        capture_sink,
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
    /// pidfd for the unreaped direct child; its `POLLIN` turns exit detection into an
    /// event instead of a bounded wait. `None` means the polling fallback is active.
    exit_fd: Option<OwnedFd>,
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
            exit_fd: open_child_pidfd(process_group),
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

    fn wait_io(&mut self, timeout: Duration, reaped: bool) -> Result<(), ProcessError> {
        let io = self.io.as_mut().expect("I/O is installed before waiting");
        // Once the primary is reaped its pidfd stays readable forever, so it must leave
        // the poll set: the loop then waits only on remaining group I/O.
        let exit_fd = if reaped { None } else { self.exit_fd.as_ref() };
        io.wait_with_exit(timeout, exit_fd)
            .map_err(ProcessError::Io)
    }

    fn cleanup(&mut self) -> Result<(), ProcessError> {
        let deadline = Instant::now() + CLEANUP_DEADLINE;
        if let Some(io) = self.io.as_mut() {
            io.close_stdin();
        }
        let termination = terminate(
            self.process_group,
            &mut self.child,
            deadline,
            self.exit_fd.as_ref(),
        );
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
                return Err(ProcessError::from(error));
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
    capture_sink: Option<Arc<dyn CaptureSink>>,
}

impl UnixIo {
    fn new(
        stdin: Option<File>,
        readers: Vec<File>,
        capture_bytes: usize,
        input: Vec<u8>,
        capture_sink: Option<Arc<dyn CaptureSink>>,
    ) -> io::Result<Self> {
        if let Some(stdin) = &stdin {
            set_nonblocking(stdin.as_raw_fd())?;
        }
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
            stdin,
            readers: readers.into_iter().map(Some).collect(),
            captures,
            buffers,
            input,
            input_offset: 0,
            capture_bytes,
            capture_sink,
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
                self.capture_sink.as_deref(),
                index,
            );
            if outcome.is_err() && result.is_ok() {
                result = outcome;
            }
        }
        result
    }

    fn wait(&self, timeout: Duration) -> io::Result<()> {
        self.wait_with_exit(timeout, None)
    }

    /// Wait for pipe readiness, and additionally for `exit_fd` to signal termination
    /// when one is supplied. The timeout still bounds every call so the caller's loop
    /// keeps checking cancellation and deadlines.
    fn wait_with_exit(&self, timeout: Duration, exit_fd: Option<&OwnedFd>) -> io::Result<()> {
        let mut descriptors =
            Vec::with_capacity(usize::from(self.stdin.is_some()) + self.readers.len() + 1);
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
        if let Some(exit_fd) = exit_fd {
            descriptors.push(libc::pollfd {
                fd: exit_fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
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
        capture_sink: Option<&dyn CaptureSink>,
        stream: usize,
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
                Ok(count) => {
                    if let Some(sink) = capture_sink {
                        sink.append(stream, &buffer[..count])?;
                    }
                    capture.push(&buffer[..count]);
                }
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

/// Poll until the process group is gone or the deadline passes; `Ok(false)`
/// means the deadline expired with the group still alive. While the primary is
/// unreaped, its pidfd is the wake source for each bounded step; once it is reaped
/// only group members remain and the sleep keeps the bounded polling cadence.
fn wait_group_exit(
    process_group: i32,
    child: &mut Child,
    deadline: Instant,
    exit_fd: Option<&OwnedFd>,
) -> io::Result<bool> {
    loop {
        let pending = child.try_wait()?.is_none();
        if !group_exists(process_group)? {
            let _ = child.wait()?;
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        match exit_fd.filter(|_| pending) {
            Some(exit_fd) => poll_exit_ready(exit_fd, Duration::from_millis(10))?,
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Block until the pidfd becomes readable (the child terminated) or `timeout` elapses.
fn poll_exit_ready(exit_fd: &OwnedFd, timeout: Duration) -> io::Result<()> {
    let mut descriptor = libc::pollfd {
        fd: exit_fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: `descriptor` is a valid one-entry array for the duration of this call.
    let result = unsafe { libc::poll(&raw mut descriptor, 1, timeout_ms) };
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

fn terminate(
    process_group: i32,
    child: &mut Child,
    cleanup_deadline: Instant,
    exit_fd: Option<&OwnedFd>,
) -> Result<(), ProcessError> {
    signal_group(process_group, libc::SIGTERM)?;
    let grace_deadline = (Instant::now() + TERM_GRACE).min(cleanup_deadline);
    if wait_group_exit(process_group, child, grace_deadline, exit_fd)? {
        return Ok(());
    }

    signal_group(process_group, libc::SIGKILL)?;
    if wait_group_exit(process_group, child, cleanup_deadline, exit_fd)? {
        Ok(())
    } else {
        Err(ProcessError::OutcomeUncertain)
    }
}

pub fn signal_group(process_group: i32, signal: i32) -> io::Result<()> {
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

pub fn group_exists(process_group: i32) -> io::Result<bool> {
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

#[cfg(target_os = "linux")]
fn open_child_pidfd(pid: i32) -> Option<OwnedFd> {
    use std::sync::atomic::{AtomicU8, Ordering};

    /// Process-wide capability probe result: unknown, available, or permanently
    /// unavailable. `pidfd_open` failing with `ENOSYS` (kernel < 5.3), `EINVAL`, or a
    /// seccomp `EPERM` is a property of the host, not of the child, so the first such
    /// failure degrades every future spawn to the polling path as well.
    const UNKNOWN: u8 = 0;
    const AVAILABLE: u8 = 1;
    const UNAVAILABLE: u8 = 2;
    static AVAILABILITY: AtomicU8 = AtomicU8::new(UNKNOWN);

    #[cfg(test)]
    if FORCE_POLL_FALLBACK.load(Ordering::Relaxed) {
        return None;
    }
    if AVAILABILITY.load(Ordering::Relaxed) == UNAVAILABLE {
        return None;
    }
    match pidfd_open(pid) {
        Ok(exit_fd) => {
            AVAILABILITY.store(AVAILABLE, Ordering::Relaxed);
            Some(exit_fd)
        }
        Err(error) => {
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOSYS | libc::EINVAL | libc::EPERM)
            ) {
                AVAILABILITY.store(UNAVAILABLE, Ordering::Relaxed);
            }
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn open_child_pidfd(_pid: i32) -> Option<OwnedFd> {
    None
}

#[cfg(target_os = "linux")]
fn pidfd_open(pid: i32) -> io::Result<OwnedFd> {
    // SAFETY: `pidfd_open` takes the target pid and flags and returns a new descriptor
    // or -1; no buffers are shared with the kernel.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let descriptor =
        i32::try_from(descriptor).map_err(|_| io::Error::other("pidfd descriptor out of range"))?;
    // SAFETY: the kernel returned a descriptor this process owns from here on.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

/// Request the kernel to deliver `SIGKILL` when the parent process exits, so an abrupt
/// owner death (SIGKILL, OOM, crash) still reaps the child tree without relying on
/// `Drop`. The `getppid` re-check closes the race where the parent dies between fork
/// and `prctl`: if reparenting already happened the pdeathsig would never fire, so the
/// child must bail out before `exec`.
#[cfg(target_os = "linux")]
fn arm_parent_death_signal() -> io::Result<()> {
    // Safety: `getppid` is a pure kernel query with no side effects.
    let ppid = unsafe { libc::getppid() };
    // SAFETY: PR_SET_PDEATHSIG with SIGKILL is a valid Linux prctl request.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // Safety: `getppid` is a pure kernel query with no side effects.
    if unsafe { libc::getppid() } != ppid {
        return Err(io::Error::other(
            "parent died before parent-death signal was armed",
        ));
    }
    Ok(())
}

/// Spawn a process tree whose lifetime outlives this call, writing both output streams
/// directly to `log`. No pipe, drain thread, or capture is created.
pub fn spawn_detached(
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
    // Safety: `pre_exec` runs the closure between `fork` and `exec`, so it may
    // only call async-signal-safe functions; `setsid`, `prctl`, and `getppid`
    // are, and the closure performs no allocation.
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
        primary_exit: None,
    })
}

pub struct DetachedTree {
    pid: u32,
    process_group: i32,
    child: Child,
    primary_exit: Option<String>,
}

impl DetachedTree {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Fallible on purpose: a failed `kill(-pgroup, 0)` probe says nothing about the tree,
    /// and callers must keep the owner rather than treat the tree as reaped.
    pub fn observe(&mut self) -> io::Result<super::DetachedObservation> {
        if self.primary_exit.is_none()
            && let Some(status) = self.child.try_wait()?
        {
            self.primary_exit = Some(exit_label(status));
        }
        let running = group_exists(self.process_group)?;
        if !running && self.primary_exit.is_none() {
            self.primary_exit = Some(exit_label(self.child.wait()?));
        }
        Ok(super::DetachedObservation {
            tree_running: running,
            primary_exit: self.primary_exit.clone(),
        })
    }

    /// Terminate the process group and confirm it died before `deadline`, sharing one
    /// deadline across every tree a shutdown owns instead of budgeting five seconds each.
    pub fn terminate_and_wait(&mut self, deadline: Instant) -> Result<(), ProcessError> {
        let outcome = terminate(self.process_group, &mut self.child, deadline, None);
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
pub fn active_pipe_workers_for_tests() -> usize {
    0
}

#[cfg(test)]
pub fn set_setup_failure_for_tests(point: SetupFailurePoint) {
    SETUP_FAILURE.set(Some(point));
    LAST_SPAWNED_PROCESS_GROUP.set(None);
}

#[cfg(test)]
pub fn take_spawned_process_group_for_tests() -> Option<i32> {
    LAST_SPAWNED_PROCESS_GROUP.take()
}

#[cfg(test)]
pub fn process_group_exists_for_tests(process_group: i32) -> bool {
    group_exists(process_group).unwrap_or(true)
}

fn record_spawn_for_tests(process_group: i32) {
    #[cfg(test)]
    LAST_SPAWNED_PROCESS_GROUP.set(Some(process_group));
    #[cfg(not(test))]
    let _ = process_group;
}

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
            Some(File::open("/dev/null").expect("null input")),
            vec![File::from(read)],
            1024,
            Vec::new(),
            None,
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

/// Behavior coverage for the pidfd-driven exit path and its permanent polling
/// fallback. Latency claims are deliberately absent: they belong to the stdio
/// benchmark's paired samples, not to clock-sensitive CI assertions.
#[cfg(all(test, target_os = "linux"))]
mod pidfd_exit_tests {
    use super::*;
    use crate::platform::process::resolve::{Launcher, ResolvedProgram};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

    fn run_shell(cwd: &Path, script: &str, timeout: Duration) -> Result<ExecOutcome, ExecFailure> {
        let resolved = ResolvedProgram {
            absolute: PathBuf::from("/bin/sh"),
            executable: PathBuf::from("/bin/sh"),
            launcher: Launcher::Native,
        };
        let args = vec!["-c".to_owned(), script.to_owned()];
        let plan = ExecPlan {
            resolved: &resolved,
            cwd,
            args: &args,
            environment: &EnvironmentPlan::default(),
            stdin: None,
            streams: Streams::Merged,
            timeout,
            capture_page_bytes: crate::output::MODEL_BYTE_LIMIT,
        };
        run(&plan, &CancellationToken::new(), None)
    }

    struct ForceFallbackGuard;

    impl Drop for ForceFallbackGuard {
        fn drop(&mut self) {
            FORCE_POLL_FALLBACK.store(false, Ordering::Relaxed);
        }
    }

    #[test]
    fn pidfd_exit_reports_the_command_status() {
        let fixture = tempfile::tempdir().expect("fixture");
        let outcome =
            run_shell(fixture.path(), "echo out; exit 7", Duration::from_secs(10)).expect("run");
        assert_eq!(outcome.exit, "7");
        let capture = &outcome.captures[0];
        let rendered = capture.render(capture.retained());
        assert!(rendered.text.contains("out"), "captured {rendered:?}");
    }

    #[test]
    fn lingering_group_members_are_reaped_after_the_primary_exits() {
        let fixture = tempfile::tempdir().expect("fixture");
        let started = Instant::now();
        let outcome = run_shell(
            fixture.path(),
            "sleep 30 & echo spawned",
            Duration::from_secs(10),
        )
        .expect("run");
        let elapsed = started.elapsed();

        assert_eq!(outcome.exit, "0");
        let capture = &outcome.captures[0];
        let rendered = capture.render(capture.retained());
        assert!(rendered.text.contains("spawned"), "captured {rendered:?}");
        // The 30-second `sleep` must die with its process group, not outlive the call
        // or hold the call open; the descendant grace is the only expected delay.
        assert!(elapsed >= DESCENDANT_EXIT_GRACE.saturating_sub(Duration::from_millis(50)));
        assert!(elapsed < Duration::from_secs(5), "cleanup took {elapsed:?}");
        let group = take_spawned_process_group_for_tests().expect("spawned group");
        assert!(
            !process_group_exists_for_tests(group),
            "the process group outlived the call"
        );
    }

    #[test]
    fn trapped_term_escapes_to_kill_inside_the_cleanup_deadline() {
        let fixture = tempfile::tempdir().expect("fixture");
        let started = Instant::now();
        let result = run_shell(
            fixture.path(),
            "trap '' TERM; echo ready; sleep 30",
            Duration::from_millis(600),
        );
        let elapsed = started.elapsed();

        assert!(
            matches!(result, Err(ExecFailure::TimedOut { .. })),
            "the hanging command must end in a bounded timeout, got {result:?}"
        );
        assert!(elapsed >= Duration::from_millis(600));
        assert!(elapsed < Duration::from_secs(4), "cleanup took {elapsed:?}");
        let group = take_spawned_process_group_for_tests().expect("spawned group");
        assert!(
            !process_group_exists_for_tests(group),
            "the process group outlived the call"
        );
    }

    #[test]
    fn repeated_spawns_leave_no_pidfd_descriptors_behind() {
        let fixture = tempfile::tempdir().expect("fixture");
        let baseline = minimum_pidfd_count();
        for _ in 0..6 {
            run_shell(fixture.path(), "true", Duration::from_secs(10)).expect("run");
        }
        // Every pidfd is owned by its `Lifecycle` and must close with it. Other tests
        // run concurrently and hold transient pidfds, so settle briefly instead of
        // asserting an instant count.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if pidfd_descriptor_count() <= baseline {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "pidfd descriptors leaked: {} > {baseline}",
                pidfd_descriptor_count()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn forced_fallback_preserves_the_polling_semantics() {
        let _guard = ForceFallbackGuard;
        FORCE_POLL_FALLBACK.store(true, Ordering::Relaxed);
        let fixture = tempfile::tempdir().expect("fixture");
        let outcome = run_shell(fixture.path(), "exit 3", Duration::from_secs(10)).expect("run");
        assert_eq!(outcome.exit, "3");
    }

    fn minimum_pidfd_count() -> usize {
        let mut minimum = pidfd_descriptor_count();
        for _ in 0..2 {
            thread::sleep(Duration::from_millis(5));
            minimum = minimum.min(pidfd_descriptor_count());
        }
        minimum
    }

    fn pidfd_descriptor_count() -> usize {
        std::fs::read_dir("/proc/self/fd")
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        std::fs::read_link(entry.path())
                            .is_ok_and(|target| target.to_string_lossy().contains("pidfd"))
                    })
                    .count()
            })
            .unwrap_or(0)
    }
}
